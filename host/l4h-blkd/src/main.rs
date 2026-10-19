//! blkd: drives one disk (virtio-blk or NVMe PCI device) for multiple clients (a
//! guest's l4h-vmm).  Requests carry guest-physical segments, which are
//! validated against the client's I/O pool and its LBA window and handed to
//! the device by physical address: the device reads and writes the pool
//! directly, never guest RAM (the guest bounces through the pool with
//! swiotlb).  See DESIGN.md §8.2.

#![no_std]
#![no_main]

extern crate alloc;

mod nvme;
mod virtio;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_rt::host;
use l4h_rt::host::blk::{self, BlkdConfig, Channel, Pool};
use l4h_rt::sel4;
use l4h_rt::{Info, cap};
use sel4::cap_type;

/// A physically contiguous piece of a request's data.
#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub pa: u64,
    pub len: u32,
}

/// Pieces of one request: one per guest segment (the pool is physically
/// contiguous).
pub const MAX_PIECES: usize = blk::MAX_SEGS;

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

struct Client {
    chan: *mut Channel,
    pool: Pool,
    window: blk::DiskWindow,
    read_only: bool,
    done: sel4::cap::Notification,
    outstanding: usize,
}

#[derive(Clone, Copy)]
struct Pending {
    client: usize,
    id: u64,
}

struct Blkd {
    clients: Vec<Option<Client>>,
    // Tags are allocated here, never taken from a client. Slots are not
    // reusable until the hardware has completed every command of a request.
    pending: Vec<Option<Pending>>,
    drv: Box<dyn Driver>,
    dev: DevInfo,
    next: usize,
    ticks_per_sec: u64,
}

impl Client {
    fn respond(&self, id: u64, status: u32) {
        unsafe {
            let h = addr_of!((*self.chan).cq.head).read_volatile();
            addr_of_mut!((*self.chan).resps[h as usize % blk::RING_SIZE])
                .write_volatile(blk::Resp { id, status, reserved: 0 });
            fence(Ordering::Release);
            addr_of_mut!((*self.chan).cq.head).write_volatile(h.wrapping_add(1));
        }
        self.done.signal();
    }

    fn prepare(&self, req: &blk::Req, block_size: u32, pieces: &mut Vec<Seg>) -> Result<u64, u32> {
        let sector = self.window.check_request(req, self.pool, block_size, self.read_only)?;
        pieces.clear();
        if req.op != blk::OP_FLUSH {
            for s in &req.segs[..req.nr_segs as usize] {
                pieces.push(Seg { pa: self.pool.translate(s.gpa, s.len as u64).unwrap(), len: s.len });
            }
        }
        Ok(sector)
    }
}

impl Blkd {
    fn reap(&mut self) -> bool {
        let mut done = Vec::new();
        self.drv.reap(&mut |tag, status| done.push((tag, status)));
        let mut provision = false;
        for (tag, status) in done {
            if tag == u64::MAX {
                provision = true;
                // Provisioning is serialized by l4h-root.
                set_mr(1, status as u64);
                continue;
            }
            if let Some(p) = self.pending.get_mut(tag as usize).and_then(|p| p.take()) {
                if let Some(c) = self.clients[p.client].as_mut() {
                    c.outstanding -= 1;
                    c.respond(p.id, status);
                }
            }
        }
        provision
    }

    fn submit(&mut self) {
        let mut pieces = Vec::with_capacity(MAX_PIECES);
        let mut started = false;
        // Bounded work per client and round-robin starting point. A busy
        // ring cannot occupy every hardware slot before others get a turn.
        for _round in 0..blk::RING_SIZE / 4 {
            for off in 0..blk::MAX_CLIENTS {
                let i = (self.next + off) % blk::MAX_CLIENTS;
                let Some(c) = self.clients[i].as_mut() else { continue };
                for _ in 0..4 {
                    if !self.drv.can_start() { break; }
                    let Some(tag) = self.pending.iter().position(Option::is_none) else { break };
                    let req = unsafe {
                        let tail = addr_of!((*c.chan).sq.tail).read_volatile();
                        let head = addr_of!((*c.chan).sq.head).read_volatile();
                        let cq_used = addr_of!((*c.chan).cq.head).read_volatile()
                            .wrapping_sub(addr_of!((*c.chan).cq.tail).read_volatile()) as usize;
                        // Reserve completion space for every in-flight request.
                        if head == tail || head.wrapping_sub(tail) as usize > blk::RING_SIZE
                            || cq_used.saturating_add(c.outstanding) >= blk::RING_SIZE { break; }
                        fence(Ordering::Acquire);
                        let req = addr_of!((*c.chan).reqs[tail as usize % blk::RING_SIZE]).read_volatile();
                        addr_of_mut!((*c.chan).sq.tail).write_volatile(tail.wrapping_add(1));
                        req
                    };
                    let result = c.prepare(&req, self.dev.block_size, &mut pieces).and_then(|sector| {
                        if req.op == blk::OP_FLUSH {
                            pieces.clear();
                            if !self.dev.flush { return Err(blk::STATUS_OK); }
                        }
                        self.drv.start(tag as u64, req.op, sector, &pieces)
                    });
                    match result {
                        Ok(()) => {
                            self.pending[tag] = Some(Pending { client: i, id: req.id });
                            c.outstanding += 1;
                            started = true;
                        }
                        Err(status) => c.respond(req.id, status),
                    }
                }
            }
        }
        self.next = (self.next + 1) % blk::MAX_CLIENTS;
        if started { self.drv.kick(); }
    }

    fn deadline(&self) -> u64 { host::ticks::now().saturating_add(5 * self.ticks_per_sec) }

    fn remove(&mut self, i: usize) -> u64 {
        if i >= self.clients.len() { return u64::MAX; }
        // Stop consuming this channel first. Polling the hardware drains
        // only submitted work; no controller reset disrupts other clients.
        let end = self.deadline();
        while self.clients[i].as_ref().is_some_and(|c| c.outstanding != 0) {
            self.reap();
            if host::ticks::now() > end { return u64::MAX; }
            core::hint::spin_loop();
        }
        self.clients[i] = None;
        0
    }

    fn provision(&mut self, cfg: &BlkdConfig, sector: u64, len: usize, flush: bool) -> u64 {
        if self.dev.read_only { return blk::STATUS_IOERR as u64; }
        if !flush {
            let Some(w) = blk::DiskWindow::new(sector, (len / 512) as u64, self.dev.capacity) else { return u64::MAX };
            if len == 0 || len > blk::STAGING_SIZE || len % self.dev.block_size as usize != 0
                || self.clients.iter().flatten().any(|c| c.window.overlaps(w)) { return u64::MAX; }
        }
        if flush && !self.dev.flush { return blk::STATUS_OK as u64; }
        let end = self.deadline();
        while !self.drv.can_start() {
            self.reap();
            if host::ticks::now() > end { return u64::MAX; }
        }
        let data = [Seg { pa: cfg.dma_paddr + cfg.dma_size, len: len as u32 }];
        let result = self.drv.start(u64::MAX, if flush { blk::OP_FLUSH } else { blk::OP_WRITE },
            sector, if flush { &[] } else { &data });
        if let Err(status) = result { return status as u64; }
        self.drv.kick();
        loop {
            if self.reap() { return mr(1); }
            if host::ticks::now() > end { return u64::MAX; }
            core::hint::spin_loop();
        }
    }
}

fn mr(i: usize) -> u64 { sel4::with_ipc_buffer(|b| b.msg_regs()[i]) }
fn set_mr(i: usize, v: u64) { sel4::with_ipc_buffer_mut(|b| b.msg_regs_mut()[i] = v); }

fn main(info: &'static Info) -> ! {
    let cfg: &BlkdConfig = unsafe { info.config() };
    let id = info.header().id;
    let drv: Box<dyn Driver> = match cfg.kind {
        blk::BLKD_NVME => Box::new(nvme::Nvme::new(id, cfg)),
        _ => Box::new(virtio::VirtioBlk::new(id, cfg)),
    };
    let dev = drv.info();
    sel4::debug_println!("blkd {:#x}: shared controller, {} sectors, {}-byte blocks", id, dev.capacity, dev.block_size);
    let mut b = Blkd {
        clients: (0..blk::MAX_CLIENTS).map(|_| None).collect(),
        pending: alloc::vec![None; 256],
        drv, dev, next: 0, ticks_per_sec: info.header().tsc_hz,
    };
    let ep = cap::<cap_type::Endpoint>(host::SLOT_SERVICE_EP);
    let reply = cap::<cap_type::Reply>(host::SLOT_REPLY);
    let irq = cap::<cap_type::IrqHandler>(blk::BLKD_SLOT_IRQ);
    let (mut msg, mut badge) = ep.recv(reply);
    loop {
        if badge != 0 {
            b.reap();
            b.submit();
            if badge & blk::BLKD_BADGE_IRQ != 0 { irq.irq_handler_ack().expect("blkd IRQ ack"); }
            (msg, badge) = ep.recv(reply);
            continue;
        }
        let result = match msg.label() {
            blk::BLKD_INFO => {
                set_mr(1, b.dev.block_size as u64);
                set_mr(2, b.dev.read_only as u64);
                b.dev.capacity
            }
            blk::BLKD_ADD => {
                let i = mr(0) as usize;
                if i >= blk::MAX_CLIENTS || b.clients[i].is_some() { u64::MAX } else {
                    let chan = blk::channel_va(i) as *mut Channel;
                    let (start, count, ro, pool) = unsafe {
                        (addr_of!((*chan).lba_start).read_volatile(), addr_of!((*chan).capacity_sectors).read_volatile(),
                         addr_of!((*chan).read_only).read_volatile(), addr_of!((*chan).pool).read_volatile())
                    };
                    match blk::DiskWindow::new(start, count, b.dev.capacity) {
                        Some(window) if !b.clients.iter().flatten().any(|c| c.window.overlaps(window)) => {
                            b.clients[i] = Some(Client { chan, pool, window, read_only: ro != 0 || b.dev.read_only,
                                done: cap::<cap_type::Notification>(blk::BLKD_SLOT_CLIENT_DONE + i as u64), outstanding: 0 });
                            unsafe {
                                addr_of_mut!((*chan).read_only).write_volatile((ro != 0 || b.dev.read_only) as u32);
                                addr_of_mut!((*chan).block_size).write_volatile(b.dev.block_size);
                                fence(Ordering::Release);
                                addr_of_mut!((*chan).ready).write_volatile(1);
                            }
                            0
                        }
                        _ => u64::MAX,
                    }
                }
            }
            blk::BLKD_REMOVE => b.remove(mr(0) as usize),
            blk::BLKD_WRITE => b.provision(cfg, mr(0), mr(1) as usize, false),
            blk::BLKD_FLUSH => b.provision(cfg, 0, 0, true),
            _ => u64::MAX,
        };
        set_mr(0, result);
        // Service calls can have reaped completions while polling. A kick
        // may have been coalesced, so always revisit all pending channels.
        b.submit();
        (msg, badge) = ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, 3), reply);
    }
}

l4h_rt::component!(main);
