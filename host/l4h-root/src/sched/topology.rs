//! Physical cores indexed by seL4 CPU number. CPU numbers and APIC IDs
//! need not be contiguous or ordered by SMT sibling.

/// CPUs a topology can describe: at least seL4's KernelMaxNumNodes in
/// either configuration (sched.rs checks it).
pub const MAX_CPUS: usize = 64;

pub struct Topology {
    cpus: usize,
    /// The first seL4 CPU belonging to each CPU's physical core.
    physical: [u8; MAX_CPUS],
    /// One thread of every physical core, then the second threads, etc.
    order: [u8; MAX_CPUS],
}

impl Topology {
    pub fn flat(cpus: usize) -> Self {
        assert!(cpus <= MAX_CPUS, "{} CPUs, at most {}", cpus, MAX_CPUS);
        let mut t = Self { cpus, physical: [0; MAX_CPUS], order: [0; MAX_CPUS] };
        for cpu in 0..cpus {
            t.physical[cpu] = cpu as u8;
            t.order[cpu] = cpu as u8;
        }
        t
    }

    pub fn from_ids(ids: &[u32]) -> Self {
        let mut t = Self::flat(ids.len());
        for (cpu, id) in ids.iter().enumerate() {
            t.physical[cpu] = ids.iter().position(|other| other == id).unwrap() as u8;
        }
        let physical = &t.physical[..t.cpus];
        // The keys are distinct, so an unstable sort gives the one order.
        t.order[..t.cpus].sort_unstable_by_key(|&cpu| {
            let cpu = cpu as usize;
            (physical[..cpu].iter().filter(|&&p| p == physical[cpu]).count(), physical[cpu])
        });
        t
    }

    /// Each CPU's physical core (its first seL4 CPU).
    pub fn physical(&self) -> &[u8] {
        &self.physical[..self.cpus]
    }

    pub fn core(&self, cpu: usize) -> usize {
        self.physical[cpu] as usize
    }

    pub fn same_core(&self, a: usize, b: usize) -> bool {
        self.core(a) == self.core(b)
    }

    pub fn placement(&self, n: usize) -> usize {
        self.order[n % self.cpus] as usize
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

/// The CPUID leaves physical_id_from reads, which the probe reads on each
/// CPU (subleaf 0 of each).
#[cfg(all(target_arch = "x86_64", not(test)))]
const PROBE_LEAVES: [u32; 6] = [0, 0x0b, 0x1f, 0x8000_0000, 0x8000_0001, 0x8000_001e];

/// The probe's page: the leaves to read, then what CPUID returned for
/// each (eax, ebx, ecx, edx), as l4h_cpuid_probe lays them out.
#[cfg(all(target_arch = "x86_64", not(test)))]
#[repr(C)]
struct ProbePage {
    n: u64,
    queries: [(u32, u32); PROBE_LEAVES.len()],
    results: [[u32; 4]; PROBE_LEAVES.len()],
}

// The probe: CPUID of each (leaf, subleaf) of the page at rdi, then a
// fault (ud2) for l4h-root to collect the results.  It uses no stack and
// no memory but that page, and has a code page of its own, so that it
// can run in a VSpace that maps nothing else.
#[cfg(all(target_arch = "x86_64", not(test)))]
core::arch::global_asm!(
    r#"
    .pushsection .text.l4h_cpuid_probe, "ax", @progbits
    .balign 4096
    .global l4h_cpuid_probe
l4h_cpuid_probe:
    mov r8, [rdi]
    lea r9, [rdi + 8]
    lea r10, [r9 + r8 * 8]
2:
    test r8, r8
    jz 3f
    mov eax, [r9]
    mov ecx, [r9 + 4]
    cpuid
    mov [r10], eax
    mov [r10 + 4], ebx
    mov [r10 + 8], ecx
    mov [r10 + 12], edx
    add r9, 8
    add r10, 16
    dec r8
    jmp 2b
3:
    ud2
    .balign 4096
    .popsection
    "#
);

/// Read CPUID on each CPU before starting any components, which avoids
/// assumptions about seL4's enumeration or disabled threads.
///
/// Root itself never leaves its boot CPU to do it: on x86_64 every CPU a
/// VSpace has run on stays in its TLB bitmap until a flush of its whole
/// PCID (when a page table is unmapped, or the ASID goes), which root's
/// VSpace never has, and each unmap of a page in it interrupts all those
/// CPUs and waits for their answers.  Root unmaps a
/// page for every page it copies into a guest (window.rs), and on a busy
/// hypervisor, whose vCPUs wait to be scheduled, a guest's boot slowed
/// from 0.2 s to tens of seconds, and l4h appeared to hang.  So a probe
/// thread in a VSpace of its own, mapping only the probe's code and its
/// page, visits the CPUs, and is destroyed after.
#[cfg(all(target_arch = "x86_64", not(test)))]
pub fn discover(bootinfo: &sel4::BootInfoPtr, alloc: &mut crate::mem::Alloc, win: &mut crate::window::Window) -> Topology {
    let cpus = bootinfo.inner().numNodes as usize;
    let mut ids = [0; MAX_CPUS];
    let mut apics = [0; MAX_CPUS];
    let mut n = 0;
    probe_cpus(bootinfo, alloc, win, |cpu, page| {
        let leaf = |leaf: u32, sub: u32| {
            let i = PROBE_LEAVES.iter().position(|&l| l == leaf).filter(|_| sub == 0).expect("CPUID leaf not probed");
            let [eax, ebx, ecx, edx] = page.results[i];
            core::arch::x86_64::CpuidResult { eax, ebx, ecx, edx }
        };
        let Some((apic, id)) = physical_id_from(leaf) else { return false };
        if n != cpu || apics[..n].contains(&apic) {
            return false;
        }
        apics[n] = apic;
        ids[n] = id;
        n += 1;
        true
    });
    if n == cpus {
        Topology::from_ids(&ids[..n])
    } else {
        sel4::debug_println!("l4h-root: CPU topology unavailable; using flat placement");
        Topology::flat(cpus)
    }
}

/// Run the probe on each CPU in turn, passing its page to `f`, until `f`
/// returns false or a CPU cannot be reached.
#[cfg(all(target_arch = "x86_64", not(test)))]
fn probe_cpus(
    bootinfo: &sel4::BootInfoPtr,
    alloc: &mut crate::mem::Alloc,
    win: &mut crate::window::Window,
    mut f: impl FnMut(usize, &ProbePage) -> bool,
) {
    use sel4::init_thread::{Slot, slot};
    use sel4::{CNodeCapData, CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

    use crate::mem::ObjCtx;
    use crate::vspace::{FRAME_BP, PML4_BP, map_frame};

    unsafe extern "C" {
        fn l4h_cpuid_probe();
    }
    /// Where the probe's VSpace maps its page.
    const PAGE_VA: usize = 0x1000_0000;
    let abs = |s: usize| slot::CNODE.cap().absolute_cptr(Slot::<cap_type::Null>::from_index(s).cap());

    // Its slots are recorded as guest 0's, which does not exist yet, to
    // free them all after.
    alloc.slots.track(0);
    let Some(mut arena) = alloc.arena(64 << 10) else {
        alloc.slots.untrack();
        return;
    };
    let mut ctx = ObjCtx::arena(alloc, &mut arena);
    let vspace = ctx.cap::<cap_type::VSpace>(PML4_BP);
    let tcb = ctx.cap::<cap_type::Tcb>(ObjectBlueprint::Tcb);
    let sc = ctx.cap::<cap_type::SchedContext>(ObjectBlueprint::SchedContext {
        size_bits: sel4::sys::seL4_MinSchedContextBits as usize,
    });
    let ep = ctx.cap::<cap_type::Endpoint>(ObjectBlueprint::Endpoint);
    let reply = ctx.cap::<cap_type::Reply>(ObjectBlueprint::Reply);
    let page = ctx.cap::<cap_type::Granule>(FRAME_BP);
    slot::ASID_POOL.cap().asid_pool_assign(vspace).expect("probe ASID");
    // The probe's code page, one of root's image frames.
    let entry = l4h_cpuid_probe as *const () as usize;
    let image = bootinfo.user_image_frames().index((entry - crate::layout::image_base()) / 4096);
    let code = ctx.alloc.slots.alloc();
    abs(code).copy(&slot::CNODE.cap().absolute_cptr(image.cap()), CapRights::read_only()).expect("probe code copy");
    map_frame(&mut ctx, vspace, Slot::<cap_type::Granule>::from_index(code).cap(), entry, CapRights::read_only(), VmAttributes::DEFAULT);
    map_frame(&mut ctx, vspace, page, PAGE_VA, CapRights::read_write(), VmAttributes::DEFAULT);
    drop(ctx);
    tcb.tcb_configure(slot::CNODE.cap(), CNodeCapData::new(0, 0), vspace, 0, cap::Granule::from_bits(0))
        .expect("probe TCB configure");

    let base = bootinfo.sched_control().range().start;
    for cpu in 0..bootinfo.inner().numNodes as usize {
        let control = Slot::<cap_type::SchedControl>::from_index(base + cpu).cap();
        if control.sched_control_configure_flags(sc, 5000, 5000, 0, 0, 0).is_err() {
            break;
        }
        if cpu == 0 {
            tcb.tcb_set_sched_params(slot::TCB.cap(), 0, l4h_abi::PRIO_KERNEL + 2, sc, ep)
                .expect("probe sched params");
        }
        win.with_frame(&slot::CNODE.cap().absolute_cptr(page), |p| unsafe {
            let p = p.cast::<ProbePage>();
            (*p).n = PROBE_LEAVES.len() as u64;
            (*p).queries = PROBE_LEAVES.map(|l| (l, 0));
        });
        let mut regs = sel4::UserContext::default();
        *regs.pc_mut() = entry as u64;
        *regs.c_param_mut(0) = PAGE_VA as u64;
        tcb.tcb_write_all_registers(true, &mut regs).expect("probe start");
        let (info, _) = ep.recv(reply);
        let done = sel4::with_ipc_buffer(|b| matches!(sel4::Fault::new(b, &info), sel4::Fault::UserException(_)));
        tcb.tcb_suspend().expect("probe stop");
        if !done || !win.with_frame(&slot::CNODE.cap().absolute_cptr(page), |p| f(cpu, unsafe { &*p.cast::<ProbePage>() })) {
            break;
        }
    }
    // Destroying the objects frees the ASID with the VSpace; the arena's
    // memory goes back to the free pool, in untracked slots.
    alloc.slots.untrack();
    alloc.reclaim(arena);
    alloc.slots.free_owned(0);
}

#[cfg(all(target_arch = "aarch64", not(test)))]
pub fn discover(bootinfo: &sel4::BootInfoPtr, _alloc: &mut crate::mem::Alloc, _win: &mut crate::window::Window) -> Topology {
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
        assert_eq!(t.physical(), [0, 1, 0, 3, 1, 0]);
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
        let _ = physical_id_from(core::arch::x86_64::__cpuid_count);
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
