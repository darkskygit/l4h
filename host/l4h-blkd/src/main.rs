//! blkd: drives one disk (virtio-blk or NVMe PCI device) for one client (a
//! guest's l4h-vmm).  Requests carry guest-physical segments, which are
//! validated against the client's RAM and its LBA window and handed to the
//! device by physical address: the device reads and writes guest memory
//! directly.  See DESIGN.md §8.2.

#![no_std]
#![no_main]

extern crate alloc;

mod nvme;
mod virtio;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_rt::host::blk::{self, BlkdConfig, Channel};
use l4h_rt::host::{self, RamChunk};
use l4h_rt::sel4;
use l4h_rt::{Info, cap};
use sel4::cap_type;

/// A physically contiguous piece of a request's data.
#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub pa: u64,
    pub len: u32,
}

/// Pieces of one request: each guest segment may split at a RAM chunk
/// boundary.
pub const MAX_PIECES: usize = 2 * blk::MAX_SEGS;

pub struct DevInfo {
    /// In 512-byte sectors.
    pub capacity: u64,
    pub block_size: u32,
    pub read_only: bool,
    pub flush: bool,
}

/// A disk driver.  Requests are identified by the caller's tag.
pub trait Driver {
    fn info(&self) -> DevInfo;
    /// Room for one more request.
    fn can_start(&self) -> bool;
    /// Start a request: `op` is blk::OP_READ, OP_WRITE or OP_FLUSH,
    /// `sector` in 512-byte units, already checked against the capacity
    /// and aligned to the block size.  Errors are blk::STATUS_*.
    fn start(&mut self, tag: u64, op: u32, sector: u64, data: &[Seg]) -> Result<(), u32>;
    /// Tell the device about started requests.
    fn kick(&mut self);
    /// Report completed requests.
    fn reap(&mut self, done: &mut dyn FnMut(u64, u32));
}

struct Blkd {
    chan: *mut Channel,
    ram: Vec<RamChunk>,
    drv: Box<dyn Driver>,
    lba_start: u64,
    /// Size of the client's window in sectors.
    sectors: u64,
    block_size: u32,
    read_only: bool,
    flush: bool,
    done: sel4::cap::Notification,
}

impl Blkd {
    /// Translate guest-physical segments into physical pieces.
    fn translate(&self, req: &blk::Req, out: &mut Vec<Seg>) -> bool {
        out.clear();
        for s in &req.segs[..req.nr_segs as usize] {
            let (mut gpa, mut len) = (s.gpa, s.len as u64);
            while len > 0 {
                let Some(c) = self.ram.iter().find(|c| gpa >= c.gpa && gpa < c.gpa + c.size) else {
                    return false;
                };
                let n = len.min(c.gpa + c.size - gpa);
                if out.len() == MAX_PIECES {
                    return false;
                }
                out.push(Seg { pa: c.paddr + (gpa - c.gpa), len: n as u32 });
                gpa += n;
                len -= n;
            }
        }
        true
    }

    fn respond(&self, id: u64, status: u32) {
        unsafe {
            let h = addr_of!((*self.chan).cq.head).read_volatile();
            addr_of_mut!((*self.chan).resps[h as usize % blk::RING_SIZE]).write_volatile(blk::Resp { id, status, reserved: 0 });
            fence(Ordering::Release);
            addr_of_mut!((*self.chan).cq.head).write_volatile(h.wrapping_add(1));
        }
    }

    /// Validate and start one request.
    fn start(&mut self, req: &blk::Req, pieces: &mut Vec<Seg>) -> Result<(), u32> {
        if req.nr_segs as usize > blk::MAX_SEGS {
            return Err(blk::STATUS_IOERR);
        }
        let device_writes = match req.op {
            blk::OP_READ => true,
            blk::OP_WRITE if self.read_only => return Err(blk::STATUS_IOERR),
            blk::OP_WRITE => false,
            blk::OP_FLUSH if !self.flush => return Err(blk::STATUS_OK),
            blk::OP_FLUSH => return self.drv.start(req.id, blk::OP_FLUSH, 0, &[]),
            _ => return Err(blk::STATUS_UNSUPP),
        };
        let segs = &req.segs[..req.nr_segs as usize];
        // Data direction must match the operation.
        if segs.iter().any(|s| (s.flags & blk::SEG_WRITE != 0) != device_writes) {
            return Err(blk::STATUS_IOERR);
        }
        let bytes: u64 = segs.iter().map(|s| s.len as u64).sum();
        let bs = self.block_size as u64;
        // The sector comes from the client: check its range first.
        if req.sector.checked_add(bytes / 512).is_none_or(|end| end > self.sectors) {
            return Err(blk::STATUS_IOERR);
        }
        // The sector on the disk must be block aligned too: drivers that
        // address blocks would round it down, outside the window.
        let sector = self.lba_start + req.sector;
        if bytes == 0 || bytes % bs != 0 || (sector * 512) % bs != 0 {
            return Err(blk::STATUS_IOERR);
        }
        if !self.translate(req, pieces) {
            return Err(blk::STATUS_IOERR);
        }
        self.drv.start(req.id, req.op, sector, pieces)
    }

    /// Take requests from the channel while the device has room.
    fn submit(&mut self) -> bool {
        let mut started = false;
        let mut responded = false;
        let mut pieces = Vec::with_capacity(MAX_PIECES);
        while self.drv.can_start() {
            let req = unsafe {
                let tail = addr_of!((*self.chan).sq.tail).read_volatile();
                if tail == addr_of!((*self.chan).sq.head).read_volatile() {
                    break;
                }
                fence(Ordering::Acquire);
                let r = addr_of!((*self.chan).reqs[tail as usize % blk::RING_SIZE]).read_volatile();
                addr_of_mut!((*self.chan).sq.tail).write_volatile(tail.wrapping_add(1));
                r
            };
            match self.start(&req, &mut pieces) {
                Ok(()) => started = true,
                Err(status) => {
                    self.respond(req.id, status);
                    responded = true;
                }
            }
        }
        if started {
            self.drv.kick();
        }
        responded
    }

    /// Reap completed requests.
    fn reap(&mut self) -> bool {
        let mut done = Vec::new();
        self.drv.reap(&mut |tag, status| done.push((tag, status)));
        for &(tag, status) in &done {
            self.respond(tag, status);
        }
        !done.is_empty()
    }
}

fn main(info: &'static Info) -> ! {
    let cfg: &BlkdConfig = unsafe { info.config() };
    let id = info.header().id;
    let drv: Box<dyn Driver> = match cfg.kind {
        blk::BLKD_NVME => Box::new(nvme::Nvme::new(id, cfg)),
        _ => Box::new(virtio::VirtioBlk::new(id, cfg)),
    };
    let dev = drv.info();
    let lba_start = cfg.lba_start.min(dev.capacity);
    let mut sectors = dev.capacity - lba_start;
    if cfg.lba_count != 0 {
        sectors = sectors.min(cfg.lba_count);
    }
    // The device addresses whole blocks: the window must start on one
    // (l4h-root checks this too) and is cut to whole blocks.
    let block_sectors = (dev.block_size as u64 / 512).max(1);
    if lba_start % block_sectors != 0 {
        sel4::debug_println!(
            "blkd {:#x}: window start {} is not aligned to {}-byte blocks; disk disabled",
            id, lba_start, dev.block_size
        );
        sectors = 0;
    }
    sectors -= sectors % block_sectors;
    let read_only = dev.read_only || cfg.read_only != 0;
    let chan = cfg.chan_va as *mut Channel;
    unsafe {
        addr_of_mut!((*chan).capacity_sectors).write_volatile(sectors);
        addr_of_mut!((*chan).read_only).write_volatile(read_only as u32);
        addr_of_mut!((*chan).block_size).write_volatile(dev.block_size);
        fence(Ordering::Release);
        addr_of_mut!((*chan).ready).write_volatile(1);
    }
    sel4::debug_println!(
        "blkd {:#x}: {:02x}:{:02x}.{} {}: {} MiB from sector {}, {}-byte blocks{}",
        id,
        cfg.pci_bdf >> 8,
        (cfg.pci_bdf >> 3) & 0x1f,
        cfg.pci_bdf & 7,
        if cfg.kind == blk::BLKD_NVME { "nvme" } else { "virtio-blk" },
        sectors >> 11,
        lba_start,
        dev.block_size,
        if read_only { ", read-only" } else { "" }
    );
    let mut b = Blkd {
        chan,
        ram: cfg.ram_chunks[..cfg.nr_ram_chunks as usize].to_vec(),
        drv,
        lba_start,
        sectors,
        block_size: dev.block_size,
        read_only,
        flush: dev.flush,
        done: cap::<cap_type::Notification>(blk::BLKD_SLOT_CLIENT_DONE),
    };

    let ntfn = cap::<cap_type::Notification>(host::SLOT_NTFN);
    let irq = cap::<cap_type::IrqHandler>(blk::BLKD_SLOT_IRQ);
    // The client is waiting for the channel to be ready.
    b.done.signal();
    loop {
        let (_, badge) = ntfn.wait();
        let mut signal = b.reap();
        signal |= b.submit();
        if badge & blk::BLKD_BADGE_IRQ != 0 {
            irq.irq_handler_ack().expect("blkd: IRQ ack");
        }
        if signal {
            b.done.signal();
        }
    }
}

l4h_rt::component!(main);
