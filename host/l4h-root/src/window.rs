//! Scratch mappings in the root task's own VSpace used to read and write
//! frames that belong to guests.

use sel4::init_thread::slot;
use sel4::{CapRights, VmAttributes, cap_type};

use crate::layout;
use crate::mem::{Alloc, ObjCtx, Slot};
use crate::vspace::map_frame;

pub struct Window {
    slot: Slot,
    va: usize,
}

impl Window {
    /// Window `n` (0 or 1) of the two the root task uses.
    pub fn new(alloc: &mut Alloc, n: usize) -> Self {
        assert!(n < 2);
        let va = layout::WINDOWS.base + n * 0x1000;
        let slot = alloc.slots.alloc();
        // Pre-create the paging structures for the window so that mapping
        // never has to allocate.
        let probe = alloc.object_cap::<cap_type::Granule>(crate::vspace::FRAME_BP);
        let mut ctx = ObjCtx::global(alloc);
        map_frame(&mut ctx, slot::VSPACE.cap(), probe, va, CapRights::read_write(), VmAttributes::DEFAULT);
        probe.frame_unmap().unwrap();
        Self { slot, va }
    }

    /// Temporarily map the frame `frame` (a capability anywhere reachable
    /// from the root CSpace) and run `f` on its contents.
    pub fn with_frame<R>(&mut self, frame: &sel4::AbsoluteCPtr, f: impl FnOnce(*mut u8) -> R) -> R {
        self.with_frame_attrs(frame, VmAttributes::DEFAULT, f)
    }

    /// `with_frame` with mapping attributes `attrs` (device registers are
    /// mapped uncached).
    pub fn with_frame_attrs<R>(&mut self, frame: &sel4::AbsoluteCPtr, attrs: VmAttributes, f: impl FnOnce(*mut u8) -> R) -> R {
        let dst = slot::CNODE.cap().absolute_cptr(sel4::init_thread::Slot::<cap_type::Null>::from_index(self.slot).cap());
        dst.copy(frame, CapRights::read_write()).expect("window copy");
        let cap = sel4::init_thread::Slot::<cap_type::Granule>::from_index(self.slot).cap();
        cap.frame_map(slot::VSPACE.cap(), self.va, CapRights::read_write(), attrs).expect("window map");
        let r = f(self.va as *mut u8);
        // ELF loading writes through this alias. Make those instructions
        // visible before another VSpace (or core) starts executing them.
        #[cfg(target_arch = "aarch64")]
        if attrs.has(VmAttributes::PAGE_CACHEABLE) {
            let error = sel4::with_ipc_buffer_mut(|ipc| {
                ipc.inner_mut().seL4_ARM_Page_Unify_Instruction(cap.bits(), 0, 4096)
            });
            assert_eq!(error, 0, "window instruction-cache maintenance");
        }
        cap.frame_unmap().unwrap();
        dst.delete().unwrap();
        r
    }
}

/// The root task's two windows.
pub struct Windows {
    pub a: Window,
    pub b: Window,
}
