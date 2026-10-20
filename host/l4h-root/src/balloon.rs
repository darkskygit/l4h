//! virtio-balloon free page reporting (DESIGN.md §15.4).  When l4h runs in
//! a VM whose hypervisor offers it (QEMU's `free-page-reporting=on`), the
//! root task tells the hypervisor which memory is free, so that the
//! hypervisor can drop it and back it again only when it is used.
//!
//! Free memory is the allocator's free blocks (`mem::Alloc`), which decides
//! what to report and when; this is only the device.  A report is one
//! chain of device-writable buffers, one per block (or per 2 GiB of one).
//! The device returns the chain when it is done with it, and the blocks
//! must not be used before.  Their contents are lost: they read as zero
//! afterwards, as seL4 would make them anyway when it hands them out again.
//!
//! There is no interrupt: the root task's timer polls for the end of a
//! report, and an allocation that needs a block in flight waits for it.

use l4h_virtio::{self as virtio, DESC_F_NEXT, DESC_F_WRITE, Dma, Mmio, NO_VECTOR, Virtq};
use sel4::CapRights;
use sel4::init_thread::slot;

use crate::blkd::DEVICE_ATTRS;
use crate::devmem::DevMem;
use crate::mem::{Alloc, Dest, ObjCtx};
use crate::pci::{self, Pci};
use crate::vspace::{FRAME_BP, map_frame};

const VIRTIO_VENDOR: u16 = 0x1af4;
const DEVICE_MODERN: u16 = 0x1045;
const DEVICE_TRANSITIONAL: u16 = 0x1002;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const F_STATS_VQ: u64 = 1 << 1;
const F_FREE_PAGE_HINT: u64 = 1 << 3;
const F_REPORTING: u64 = 1 << 5;

/// The device's BARs are mapped at BALLOON_VA + bar * BAR_STRIDE, its
/// virtqueue after them.
const BALLOON_VA: usize = 0x0000_7400_0000_0000;
const BAR_STRIDE: usize = 0x100_0000;
const DMA_VA: usize = BALLOON_VA + 6 * BAR_STRIDE;
/// Descriptors, available and used ring, a page each.
const DMA_PAGES: usize = 3;
const PAGE: usize = 4096;

/// Blocks smaller than this (2 MiB, the host's huge pages) are not reported.
pub const MIN_BITS: usize = 21;
/// Most bytes in one descriptor (lengths are 32 bits).
pub const MAX_DESC_BITS: usize = 31;
/// A block counts as free once it stayed free this long (as Linux does),
/// so that memory freed and soon allocated again is not reported.
pub const REPORT_DELAY_MS: u64 = 2000;
/// Longest wait for the device to return a report.
const WAIT_MS: u64 = 2000;

pub fn is_balloon(d: &pci::Device) -> bool {
    d.vendor == VIRTIO_VENDOR
        && (d.device == DEVICE_MODERN || d.device == DEVICE_TRANSITIONAL)
        && d.virtio_caps.iter().any(|c| c.cfg_type == VIRTIO_PCI_CAP_COMMON_CFG)
}

pub struct Balloon {
    vq: Virtq,
    busy: bool,
    ticks_per_ms: u64,
}

impl Balloon {
    /// Set up the device for free page reporting.  None if it cannot
    /// report (the hypervisor did not enable it), or is unusable.
    pub fn new(d: &pci::Device, pci: &Pci, alloc: &mut Alloc, devmem: &mut DevMem, ticks_per_ms: u64) -> Option<Self> {
        let cap = |t: u8| d.virtio_caps.iter().find(|c| c.cfg_type == t).copied();
        let (common, notify, device) =
            (cap(VIRTIO_PCI_CAP_COMMON_CFG)?, cap(VIRTIO_PCI_CAP_NOTIFY_CFG)?, cap(VIRTIO_PCI_CAP_DEVICE_CFG)?);
        let mut mapped = alloc::vec::Vec::new();
        for c in [common, notify, device] {
            if !mapped.contains(&c.bar) {
                map_bar(d, c.bar, alloc, devmem)?;
                mapped.push(c.bar);
            }
        }
        let va = |c: pci::VirtioCap| BALLOON_VA + c.bar as usize * BAR_STRIDE + c.offset as usize;
        let dev = virtio::Device {
            common: Mmio(va(common)),
            notify: Mmio(va(notify)),
            notify_mult: notify.notify_off_multiplier,
            device: Mmio(va(device)),
        };

        alloc.refill();
        let frames = alloc.slots.global(|s| s.alloc_range(DMA_PAGES));
        let paddr = alloc.pool.retype(&FRAME_BP, DMA_PAGES, Dest::root(frames.start))?;
        for (i, s) in frames.enumerate() {
            let f = sel4::init_thread::Slot::<sel4::cap_type::Granule>::from_index(s).cap();
            map_frame(&mut ObjCtx::global(alloc), slot::VSPACE.cap(), f, DMA_VA + i * PAGE,
                CapRights::read_write(), sel4::VmAttributes::DEFAULT);
        }
        let mut dma = Dma::new(DMA_VA, paddr as u64, DMA_PAGES * PAGE);

        pci.enable(d);
        dev.reset();
        let offered = dev.features();
        if offered & virtio::F_VERSION_1 == 0 || offered & F_REPORTING == 0 {
            info!("l4h-root: balloon: no free page reporting (features {:#x})", offered);
            dev.fail();
            pci.disable(d);
            return None;
        }
        if !dev.set_features(virtio::F_VERSION_1 | F_REPORTING | (offered & F_STATS_VQ)) {
            dev.fail();
            pci.disable(d);
            return None;
        }
        // Queues: inflate, deflate, then the statistics and free page
        // hinting queues if the device has them, then reporting.  QEMU
        // numbers its queues by what it offers, whatever is negotiated;
        // the statistics queue is negotiated too, so that a device that
        // numbers them by what is negotiated agrees.
        let idx = 2 + (offered & F_STATS_VQ != 0) as u16 + (offered & F_FREE_PAGE_HINT != 0) as u16;
        let Some(vq) = dev.setup_queue(&mut dma, idx, 32, NO_VECTOR) else {
            dev.fail();
            pci.disable(d);
            return None;
        };
        dev.driver_ok();
        info!("l4h-root: balloon: free page reporting on queue {}, {} buffers", idx, vq.num);
        Some(Balloon { vq, busy: false, ticks_per_ms })
    }

    /// Buffers one report can hold.
    pub fn capacity(&self) -> usize {
        self.vq.num as usize
    }

    /// A report is in flight.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Report `ranges` (physical address, length) as free.  Nothing may
    /// use them until `done`.
    pub fn report(&mut self, ranges: &[(usize, usize)]) {
        assert!(!self.busy && !ranges.is_empty() && ranges.len() <= self.capacity());
        for (i, &(addr, len)) in ranges.iter().enumerate() {
            assert!(len < 1 << 32);
            let next = i + 1 < ranges.len();
            let flags = DESC_F_WRITE | if next { DESC_F_NEXT } else { 0 };
            self.vq.set_desc(i as u16, addr as u64, len as u32, flags, if next { i as u16 + 1 } else { 0 });
        }
        self.vq.submit(0);
        self.vq.notify();
        self.busy = true;
    }

    /// Whether the report in flight (if any) is done; `wait` waits for it,
    /// for a while.
    pub fn done(&mut self, wait: bool) -> bool {
        if !self.busy {
            return true;
        }
        let end = l4h_host::ticks::now() + WAIT_MS * self.ticks_per_ms;
        loop {
            if self.vq.pop_used().is_some() {
                self.busy = false;
                return true;
            }
            if !wait || l4h_host::ticks::now() > end {
                return false;
            }
            core::hint::spin_loop();
        }
    }
}

/// Map BAR `b` of `d` into the root task, uncached.
fn map_bar(d: &pci::Device, b: u8, alloc: &mut Alloc, devmem: &mut DevMem) -> Option<()> {
    let bar = d.bars[b as usize];
    if bar.is_io || bar.size == 0 || bar.size as usize > BAR_STRIDE {
        return None;
    }
    let frames = alloc.slots.global(|s| devmem.frames(s, bar.addr as usize, bar.size as usize))?;
    for (i, f) in frames.into_iter().enumerate() {
        map_frame(&mut ObjCtx::global(alloc), slot::VSPACE.cap(), f, BALLOON_VA + b as usize * BAR_STRIDE + i * PAGE,
            CapRights::read_write(), DEVICE_ATTRS);
    }
    Some(())
}
