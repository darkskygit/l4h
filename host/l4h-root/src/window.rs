//! A scratch mapping in the root task's own VSpace used to read and write
//! frames that belong to guests.

use sel4::init_thread::slot;
use sel4::{CapRights, VmAttributes, cap_type};

use crate::mem::{Alloc, ObjCtx, Slot};
use crate::vspace::map_frame;

pub const WINDOW_VA: usize = 0x0000_7000_0000_0000;

pub struct Window {
    slot: Slot,
}

impl Window {
    pub fn new(alloc: &mut Alloc) -> Self {
        let slot = alloc.slots.alloc();
        // Pre-create the paging structures for the window so that mapping
        // never has to allocate.
        let probe = alloc.object_cap::<cap_type::Granule>(crate::vspace::FRAME_BP);
        let mut ctx = ObjCtx { slots: &mut alloc.slots, pool: &mut alloc.pool };
        map_frame(&mut ctx, slot::VSPACE.cap(), probe, WINDOW_VA, CapRights::read_write(), VmAttributes::DEFAULT);
        probe.frame_unmap().unwrap();
        Self { slot }
    }

    /// Temporarily map the frame `frame` (a capability anywhere reachable
    /// from the root CSpace) and run `f` on its contents.
    pub fn with_frame<R>(&mut self, alloc: &mut Alloc, frame: &sel4::AbsoluteCPtr, f: impl FnOnce(*mut u8) -> R) -> R {
        let dst = slot::CNODE.cap().absolute_cptr(sel4::init_thread::Slot::<cap_type::Null>::from_index(self.slot).cap());
        dst.copy(frame, CapRights::read_write()).expect("window copy");
        let cap = sel4::init_thread::Slot::<cap_type::Granule>::from_index(self.slot).cap();
        let mut ctx = ObjCtx { slots: &mut alloc.slots, pool: &mut alloc.pool };
        map_frame(&mut ctx, slot::VSPACE.cap(), cap, WINDOW_VA, CapRights::read_write(), VmAttributes::DEFAULT);
        let r = f(WINDOW_VA as *mut u8);
        cap.frame_unmap().unwrap();
        dst.delete().unwrap();
        r
    }
}
