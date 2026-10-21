//! virtio-balloon free page reporting (DESIGN.md §15.4).  When l4h runs in
//! a VM whose hypervisor offers it (QEMU's `free-page-reporting=on`), memd
//! tells the hypervisor which memory is free, so that the hypervisor can
//! drop it and back it again only when it is used.
//!
//! Free memory is the free blocks (`mem::Mem`), which decide what to
//! report and when; this is only the device, which l4h-root found on PCI
//! and mapped here.  A report is one chain of device-writable buffers, one
//! per block (or per 2 GiB of one).  The device returns the chain when it
//! is done with it, and the blocks must not be used before.  Their
//! contents are lost: they read as zero afterwards, as seL4 would make
//! them anyway when it hands them out again.
//!
//! There is no interrupt: memd's timer polls for the end of a report, and
//! an allocation that needs a block in flight waits for it.

use l4h_rt::host::memd::{BalloonConfig, DMA_PAGES, DMA_VA};
use l4h_virtio::{self as virtio, DESC_F_NEXT, DESC_F_WRITE, Dma, Mmio, NO_VECTOR, Virtq};

const F_STATS_VQ: u64 = 1 << 1;
const F_FREE_PAGE_HINT: u64 = 1 << 3;
const F_REPORTING: u64 = 1 << 5;

const PAGE: usize = 4096;

/// Most bytes in one descriptor (lengths are 32 bits).
pub const MAX_DESC_BITS: usize = 31;
/// A block counts as free once it stayed free this long (as Linux does),
/// so that memory freed and soon allocated again is not reported.
pub const REPORT_DELAY_MS: u64 = 2000;
/// Buffers of the reporting queue: the most a report holds.
pub const QUEUE_SIZE: usize = 32;
/// Longest wait for the device to return a report.
const WAIT_MS: u64 = 2000;

pub struct Balloon {
    vq: Virtq,
    busy: bool,
    ticks_per_ms: u64,
}

impl Balloon {
    /// Set up the device for free page reporting.  None if it cannot
    /// report (the hypervisor did not enable it), or is unusable.
    pub fn new(cfg: &BalloonConfig, ticks_per_ms: u64) -> Option<Self> {
        let dev = virtio::Device {
            common: Mmio(BalloonConfig::va(cfg.common)),
            notify: Mmio(BalloonConfig::va(cfg.notify)),
            notify_mult: cfg.notify_mult as u32,
            device: Mmio(BalloonConfig::va(cfg.device)),
        };
        let mut dma = Dma::new(DMA_VA, cfg.dma_paddr, DMA_PAGES * PAGE);

        dev.reset();
        let offered = dev.features();
        if offered & virtio::F_VERSION_1 == 0 || offered & F_REPORTING == 0 {
            l4h_rt::info!("memd: balloon: no free page reporting (features {:#x})", offered);
            dev.fail();
            return None;
        }
        if !dev.set_features(virtio::F_VERSION_1 | F_REPORTING | (offered & F_STATS_VQ)) {
            dev.fail();
            return None;
        }
        // Queues: inflate, deflate, then the statistics and free page
        // hinting queues if the device has them, then reporting.  QEMU
        // numbers its queues by what it offers, whatever is negotiated;
        // the statistics queue is negotiated too, so that a device that
        // numbers them by what is negotiated agrees.
        let idx = 2 + (offered & F_STATS_VQ != 0) as u16 + (offered & F_FREE_PAGE_HINT != 0) as u16;
        let Some(vq) = dev.setup_queue(&mut dma, idx, QUEUE_SIZE as u16, NO_VECTOR) else {
            dev.fail();
            return None;
        };
        dev.driver_ok();
        l4h_rt::info!("memd: balloon: free page reporting on queue {}, {} buffers", idx, vq.num);
        Some(Balloon { vq, busy: false, ticks_per_ms })
    }

    /// Buffers one report can hold.
    pub fn capacity(&self) -> usize {
        (self.vq.num as usize).min(QUEUE_SIZE)
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
        let end = l4h_rt::host::ticks::now() + WAIT_MS * self.ticks_per_ms;
        loop {
            if self.vq.pop_used().is_some() {
                self.busy = false;
                return true;
            }
            if !wait || l4h_rt::host::ticks::now() > end {
                return false;
            }
            core::hint::spin_loop();
        }
    }
}
