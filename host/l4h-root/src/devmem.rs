//! Device memory: frames for physical address ranges (PCI BARs), carved out
//! of the device Untypeds by repeated halving.

use alloc::vec::Vec;

use sel4::{ObjectBlueprint, cap, cap_type};

use crate::mem::{Dest, Slots};
use crate::vspace::FRAME_BP;

struct Region {
    cap: cap::Untyped,
    paddr: usize,
    size_bits: usize,
}

pub struct DevMem {
    /// Untouched device Untypeds (boot ones and halves split off them).
    free: Vec<Region>,
}

impl DevMem {
    pub fn from_bootinfo(bootinfo: &sel4::BootInfo) -> Self {
        let mut free = Vec::new();
        for (i, desc) in bootinfo.untyped_list().iter().enumerate() {
            if desc.is_device() {
                free.push(Region {
                    cap: bootinfo.untyped().index(i).cap(),
                    paddr: desc.paddr(),
                    size_bits: desc.size_bits(),
                });
            }
        }
        DevMem { free }
    }

    /// 4 KiB frames covering [paddr, paddr + size), which must be aligned to
    /// its power-of-two size (as PCI BARs are).
    pub fn frames(&mut self, slots: &mut Slots, paddr: usize, size: usize) -> Option<Vec<cap::Granule>> {
        let size = size.max(4096).next_power_of_two();
        let bits = size.trailing_zeros() as usize;
        assert!(paddr % size == 0, "device range {:#x}+{:#x} not aligned", paddr, size);
        let i = self
            .free
            .iter()
            .position(|r| r.paddr <= paddr && paddr + size <= r.paddr + (1 << r.size_bits))?;
        let mut r = self.free.swap_remove(i);
        while r.size_bits > bits {
            let half = r.size_bits - 1;
            let s = slots.alloc_range(2);
            r.cap
                .untyped_retype(&ObjectBlueprint::Untyped { size_bits: half }, &Dest::root(s.start).cnode, s.start, 2)
                .expect("split device untyped");
            let lo = Region { cap: slot_cap(s.start), paddr: r.paddr, size_bits: half };
            let hi = Region { cap: slot_cap(s.start + 1), paddr: r.paddr + (1 << half), size_bits: half };
            if paddr < hi.paddr {
                self.free.push(hi);
                r = lo;
            } else {
                self.free.push(lo);
                r = hi;
            }
        }
        let n = size / 4096;
        let s = slots.alloc_range(n);
        r.cap.untyped_retype(&FRAME_BP, &Dest::root(s.start).cnode, s.start, n).expect("device frames");
        Some(s.map(|i| sel4::init_thread::Slot::<cap_type::Granule>::from_index(i).cap()).collect())
    }
}

fn slot_cap(i: usize) -> cap::Untyped {
    sel4::init_thread::Slot::<cap_type::Untyped>::from_index(i).cap()
}
