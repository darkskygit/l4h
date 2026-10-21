//! Physical cores indexed by seL4 CPU number. CPU numbers and APIC IDs
//! need not be contiguous or ordered by SMT sibling.

use alloc::vec::Vec;

pub struct Topology {
    /// The first seL4 CPU belonging to each CPU's physical core.
    pub physical: Vec<usize>,
    /// One thread of every physical core, then the second threads, etc.
    order: Vec<usize>,
}

impl Topology {
    pub fn flat(cpus: usize) -> Self {
        Self { physical: (0..cpus).collect(), order: (0..cpus).collect() }
    }

    pub fn from_ids(ids: &[u32]) -> Self {
        let physical: Vec<usize> = ids.iter().map(|id| ids.iter().position(|other| other == id).unwrap()).collect();
        let mut order: Vec<usize> = (0..ids.len()).collect();
        order.sort_by_key(|&cpu| (physical[..cpu].iter().filter(|&&p| p == physical[cpu]).count(), physical[cpu]));
        Self { physical, order }
    }

    pub fn same_core(&self, a: usize, b: usize) -> bool {
        self.physical[a] == self.physical[b]
    }

    pub fn placement(&self, n: usize) -> usize {
        self.order[n % self.order.len()]
    }
}

/// Decode the SMT level of CPUID.1F/0B. Keep the package bits in the ID:
/// equal core numbers in different sockets are different physical cores.
#[cfg(target_arch = "x86_64")]
fn smt_id(eax: u32, ebx: u32, ecx: u32, edx: u32) -> Option<u32> {
    if ebx & 0xffff == 0 || (ecx >> 8) & 0xff != 1 {
        return None;
    }
    // Clear thread bits instead of shifting: hybrid CPUs can report
    // different SMT widths, and their physical IDs must not collide.
    Some(edx & (u32::MAX << (eax & 0x1f)))
}

#[cfg(target_arch = "x86_64")]
fn physical_id() -> Option<(u32, u32)> {
    physical_id_from(core::arch::x86_64::__cpuid_count)
}

#[cfg(target_arch = "x86_64")]
fn physical_id_from(cpuid: impl Fn(u32, u32) -> core::arch::x86_64::CpuidResult) -> Option<(u32, u32)> {
    // Intel SDM, CPUID extended topology enumeration: prefer 1F to 0B.
    let max = cpuid(0, 0).eax;
    for leaf in [0x1f, 0x0b] {
        if max >= leaf {
            let r = cpuid(leaf, 0);
            if let Some(id) = smt_id(r.eax, r.ebx, r.ecx, r.edx) {
                return Some((r.edx, id));
            }
        }
    }
    // AMD topology extensions (Zen and later). Older AMD compute-unit
    // topology is deliberately left flat.
    let vendor = cpuid(0, 0);
    let ext = cpuid(0x8000_0000, 0).eax;
    if (vendor.ebx, vendor.edx, vendor.ecx) == (0x6874_7541, 0x6974_6e65, 0x444d_4163) && ext >= 0x8000_001e {
        let features = cpuid(0x8000_0001, 0);
        let family = ((features.eax >> 8) & 0xf) + ((features.eax >> 20) & 0xff);
        if family >= 0x17 && features.ecx & (1 << 22) != 0 {
            let r = cpuid(0x8000_001e, 0);
            let threads = ((r.ebx >> 8) & 0xff) + 1;
            let shift = threads.next_power_of_two().trailing_zeros();
            return Some((r.eax, r.eax & (u32::MAX << shift)));
        }
    }
    None
}

/// Visit the CPUs before starting any components. Reading CPUID on each
/// CPU avoids assumptions about seL4's enumeration or disabled threads.
/// Restore root's SC to its boot CPU before doing anything else.
#[cfg(all(target_arch = "x86_64", not(test)))]
pub fn discover(bootinfo: &sel4::BootInfoPtr) -> Topology {
    use sel4::{
        cap_type,
        init_thread::{Slot, slot},
    };

    let cpus = bootinfo.inner().numNodes as usize;
    let boot_cpu = bootinfo.inner().nodeID as usize;
    let base = bootinfo.sched_control().range().start;
    let move_to = |cpu| {
        Slot::<cap_type::SchedControl>::from_index(base + cpu).cap().sched_control_configure_flags(
            slot::SC.cap(),
            5000,
            5000,
            0,
            0,
            0,
        )
    };
    if physical_id().is_none() {
        return Topology::flat(cpus);
    }
    let mut ids = Vec::new();
    let mut apics = Vec::new();
    for cpu in 0..cpus {
        if move_to(cpu).is_err() {
            break;
        }
        let Some((apic, id)) = physical_id() else { break };
        if apics.contains(&apic) {
            break;
        }
        apics.push(apic);
        ids.push(id);
    }
    move_to(boot_cpu).expect("restore root CPU after topology discovery");
    if ids.len() == cpus {
        Topology::from_ids(&ids)
    } else {
        sel4::debug_println!("l4h-root: CPU topology unavailable; using flat placement");
        Topology::flat(cpus)
    }
}

#[cfg(all(target_arch = "aarch64", not(test)))]
pub fn discover(bootinfo: &sel4::BootInfoPtr) -> Topology {
    Topology::flat(bootinfo.inner().numNodes as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_spreads_before_using_siblings() {
        let t = Topology::from_ids(&[9, 9, 42, 42]);
        assert_eq!((0..8).map(|n| t.placement(n)).collect::<Vec<_>>(), [0, 2, 1, 3, 0, 2, 1, 3]);
        assert!(t.same_core(0, 1));
        assert!(!t.same_core(1, 2));
    }

    #[test]
    fn sparse_unordered_and_partial_sibling_sets() {
        let t = Topology::from_ids(&[42, 7, 42, 99, 7, 42]);
        assert_eq!(t.physical, [0, 1, 0, 3, 1, 0]);
        assert_eq!((0..6).map(|n| t.placement(n)).collect::<Vec<_>>(), [0, 1, 3, 2, 4, 5]);
        let flat = Topology::flat(4);
        assert_eq!((0..4).map(|n| flat.placement(n)).collect::<Vec<_>>(), [0, 1, 2, 3]);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn cpuid_smt_level_keeps_socket_bits_and_rejects_missing_leaves() {
        assert_eq!(smt_id(1, 2, 1 << 8, 0x103), Some(0x102));
        assert_eq!(smt_id(0, 1, 1 << 8, 7), Some(7));
        assert_ne!(smt_id(0, 1, 1 << 8, 2), smt_id(1, 2, 1 << 8, 4));
        assert_eq!(smt_id(1, 0, 1 << 8, 3), None);
        assert_eq!(smt_id(1, 2, 2 << 8, 3), None);
        // Exercise the host's CPUID path as well.
        let _ = physical_id();
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn prefer_leaf_1f_and_fall_back_to_0b_or_flat() {
        use core::arch::x86_64::CpuidResult;
        let zero = CpuidResult { eax: 0, ebx: 0, ecx: 0, edx: 0 };
        let read = |leaf, _| match leaf {
            0 => CpuidResult { eax: 0x1f, ..zero },
            0x1f => CpuidResult { eax: 1, ebx: 2, ecx: 1 << 8, edx: 0x103 },
            0x0b => CpuidResult { eax: 1, ebx: 2, ecx: 1 << 8, edx: 0x205 },
            _ => zero,
        };
        assert_eq!(physical_id_from(read), Some((0x103, 0x102)));
        assert_eq!(
            physical_id_from(|leaf, sub| if leaf == 0x1f { zero } else { read(leaf, sub) }),
            Some((0x205, 0x204))
        );
        assert_eq!(physical_id_from(|_, _| zero), None);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn amd_zen_topology_requires_the_feature_bit() {
        use core::arch::x86_64::CpuidResult;
        let zero = CpuidResult { eax: 0, ebx: 0, ecx: 0, edx: 0 };
        let read = |leaf, _| match leaf {
            0 => CpuidResult { eax: 0x10, ebx: 0x6874_7541, edx: 0x6974_6e65, ecx: 0x444d_4163 },
            0x8000_0000 => CpuidResult { eax: 0x8000_001e, ..zero },
            0x8000_0001 => CpuidResult { eax: (0xf << 8) | (8 << 20), ecx: 1 << 22, ..zero },
            0x8000_001e => CpuidResult { eax: 0x103, ebx: 1 << 8, ..zero },
            _ => zero,
        };
        assert_eq!(physical_id_from(read), Some((0x103, 0x102)));
        assert_eq!(physical_id_from(|leaf, sub| if leaf == 0x8000_0001 { zero } else { read(leaf, sub) }), None);
    }
}
