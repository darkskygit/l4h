//! blkd: drives one virtio-blk PCI device for one client (a guest's
//! l4h-vmm).  Requests carry guest-physical segments, which are validated
//! against the client's RAM and handed to the device by physical address:
//! the device reads and writes guest memory directly.  See DESIGN.md §8.2.

#![no_std]
#![no_main]

extern crate alloc;


use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_rt::host::blk::{self, BlkdConfig, Channel};
use l4h_rt::host::{self, RamChunk};
use l4h_rt::sel4;
use l4h_rt::{Info, cap};
use sel4::cap_type;

use l4h_virtio::{DESC_F_INDIRECT, DESC_F_NEXT, DESC_F_WRITE, Dma, F_INDIRECT_DESC, F_VERSION_1, Mmio, Virtq};

const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

/// Descriptors per request: header, data segments (each split at most once
/// at a chunk boundary), status.
const MAX_DESCS: usize = 2 + 2 * blk::MAX_SEGS;
const SLOT_TABLE: usize = MAX_DESCS * 16;

struct Slot {
    id: u64,
    busy: bool,
    /// Indirect table, request header and status byte.
    table_va: usize,
    table_pa: u64,
    hdr_va: usize,
    hdr_pa: u64,
}

struct Blkd {
    chan: *mut Channel,
    ram: Vec<RamChunk>,
    vq: Virtq,
    slots: Vec<Slot>,
    free: Vec<u16>,
    done: sel4::cap::Notification,
    read_only: bool,
    flush: bool,
}

impl Blkd {
    /// Translate a guest-physical segment into physical pieces.
    fn translate(&self, gpa: u64, len: u64, out: &mut impl FnMut(u64, u32) -> bool) -> bool {
        let (mut gpa, mut len) = (gpa, len);
        while len > 0 {
            let Some(c) = self.ram.iter().find(|c| gpa >= c.gpa && gpa < c.gpa + c.size) else {
                return false;
            };
            let n = len.min(c.gpa + c.size - gpa);
            if !out(c.paddr + (gpa - c.gpa), n as u32) {
                return false;
            }
            gpa += n;
            len -= n;
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

    /// Start one request on the device.  Returns the immediate status for
    /// requests that are rejected without reaching it.
    fn start(&mut self, req: &blk::Req) -> Result<(), u32> {
        let ty = match req.op {
            blk::OP_READ | blk::OP_GET_ID => req.op,
            blk::OP_WRITE if self.read_only => return Err(blk::STATUS_IOERR),
            blk::OP_WRITE => req.op,
            blk::OP_FLUSH if !self.flush => return Err(blk::STATUS_OK),
            blk::OP_FLUSH => req.op,
            _ => return Err(blk::STATUS_UNSUPP),
        };
        if req.nr_segs as usize > blk::MAX_SEGS {
            return Err(blk::STATUS_IOERR);
        }
        let s = self.free.pop().ok_or(blk::STATUS_IOERR)?;
        let slot = &self.slots[s as usize];
        let (table, hdr_va, hdr_pa) = (Mmio(slot.table_va), slot.hdr_va, slot.hdr_pa);
        let hdr = Mmio(hdr_va);
        hdr.w32(0, ty);
        hdr.w32(4, 0);
        hdr.w64(8, req.sector);
        hdr.w8(16, 0xff);

        // (address, length, flags) of each descriptor of the chain.
        let mut descs = [(0u64, 0u32, 0u16); MAX_DESCS];
        let mut n = 0usize;
        descs[n] = (hdr_pa, 16, 0);
        n += 1;
        let mut ok = true;
        for seg in &req.segs[..req.nr_segs as usize] {
            let flags = if seg.flags & blk::SEG_WRITE != 0 { DESC_F_WRITE } else { 0 };
            ok &= self.translate(seg.gpa, seg.len as u64, &mut |pa, len| {
                if n + 1 >= MAX_DESCS {
                    return false;
                }
                descs[n] = (pa, len, flags);
                n += 1;
                true
            });
        }
        if !ok {
            self.free.push(s);
            return Err(blk::STATUS_IOERR);
        }
        descs[n] = (hdr_pa + 16, 1, DESC_F_WRITE);
        n += 1;
        for (i, &(addr, len, flags)) in descs[..n].iter().enumerate() {
            let d = Mmio(table.0 + 16 * i);
            let last = i + 1 == n;
            d.w64(0, addr);
            d.w32(8, len);
            d.w16(12, if last { flags } else { flags | DESC_F_NEXT });
            d.w16(14, if last { 0 } else { (i + 1) as u16 });
        }

        let slot = &mut self.slots[s as usize];
        slot.id = req.id;
        slot.busy = true;
        self.vq.set_desc(s, slot.table_pa, (16 * n) as u32, DESC_F_INDIRECT, 0);
        self.vq.submit(s);
        Ok(())
    }

    /// Take requests from the channel while device slots are free.
    fn submit(&mut self) -> bool {
        let mut started = false;
        let mut responded = false;
        loop {
            if self.free.is_empty() {
                break;
            }
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
            match self.start(&req) {
                Ok(()) => started = true,
                Err(status) => {
                    self.respond(req.id, status);
                    responded = true;
                }
            }
        }
        if started {
            self.vq.notify();
        }
        responded
    }

    /// Reap completed requests.
    fn reap(&mut self) -> bool {
        let mut any = false;
        while let Some((id, _len)) = self.vq.pop_used() {
            let Some(slot) = self.slots.get_mut(id as usize) else { continue };
            if !slot.busy {
                continue;
            }
            slot.busy = false;
            let status = match Mmio(slot.hdr_va).r8(16) {
                0 => blk::STATUS_OK,
                2 => blk::STATUS_UNSUPP,
                _ => blk::STATUS_IOERR,
            };
            let rid = slot.id;
            self.free.push(id as u16);
            self.respond(rid, status);
            any = true;
        }
        any
    }
}

fn main(info: &'static Info) -> ! {
    let cfg: &BlkdConfig = unsafe { info.config() };
    let id = info.header().id;
    let dev = l4h_virtio::Device {
        common: Mmio(cfg.common.va as usize),
        notify: Mmio(cfg.notify.va as usize),
        notify_mult: cfg.notify_off_multiplier,
        device: Mmio(cfg.device.va as usize),
    };
    let mut dma = Dma::new(cfg.dma_va as usize, cfg.dma_paddr, cfg.dma_size as usize);

    dev.reset();
    let offered = dev.features();
    let want = F_VERSION_1 | F_INDIRECT_DESC;
    if offered & want != want {
        dev.fail();
        panic!("blkd {:#x}: device lacks VERSION_1/INDIRECT_DESC (features {:#x})", id, offered);
    }
    let features = want | (offered & (VIRTIO_BLK_F_RO | VIRTIO_BLK_F_FLUSH));
    if !dev.set_features(features) {
        dev.fail();
        panic!("blkd {:#x}: features rejected", id);
    }
    // MSI-X table entry 0 -> our interrupt, unmasked.
    let msix = Mmio(cfg.msix_table.va as usize);
    msix.w32(0, 0xfee0_0000);
    msix.w32(4, 0);
    msix.w32(8, cfg.msi_data);
    msix.w32(12, 0);
    let vq = dev.setup_queue(&mut dma, 0, blk::RING_SIZE as u16, 0).expect("blkd: queue 0");
    let capacity = dev.device.r64(0);

    let mut slots = Vec::new();
    for _ in 0..vq.num {
        let (table_va, table_pa) = dma.alloc(SLOT_TABLE, 16);
        let (hdr_va, hdr_pa) = dma.alloc(32, 32);
        slots.push(Slot { id: 0, busy: false, table_va, table_pa, hdr_va, hdr_pa });
    }
    let chan = cfg.chan_va as *mut Channel;
    unsafe {
        addr_of_mut!((*chan).capacity_sectors).write_volatile(capacity);
        addr_of_mut!((*chan).read_only).write_volatile((features & VIRTIO_BLK_F_RO != 0) as u32);
        fence(Ordering::Release);
        addr_of_mut!((*chan).ready).write_volatile(1);
    }
    let mut b = Blkd {
        chan,
        ram: cfg.ram_chunks[..cfg.nr_ram_chunks as usize].to_vec(),
        free: (0..vq.num).rev().collect(),
        vq,
        slots,
        done: cap::<cap_type::Notification>(blk::BLKD_SLOT_CLIENT_DONE),
        read_only: features & VIRTIO_BLK_F_RO != 0,
        flush: features & VIRTIO_BLK_F_FLUSH != 0,
    };
    dev.driver_ok();
    sel4::debug_println!(
        "blkd {:#x}: {:02x}:{:02x}.{}: {} MiB{}, queue size {}",
        id,
        cfg.pci_bdf >> 8,
        (cfg.pci_bdf >> 3) & 0x1f,
        cfg.pci_bdf & 7,
        capacity >> 11,
        if b.read_only { " read-only" } else { "" },
        b.vq.num
    );

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
