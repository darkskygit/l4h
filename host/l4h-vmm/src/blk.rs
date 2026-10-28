//! virtio-blk device backed by a blkd channel.  Data buffers are passed
//! to blkd by guest-physical address (they lie in the guest's I/O pool);
//! nothing is copied here.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_rt::host::blk::{self, Channel};
use l4h_rt::host::fixed::ArrayVec;
use l4h_rt::sel4;

use crate::dev::{Backend, VIRTIO_RING_F_INDIRECT_DESC};
use crate::guest::GuestMem;
use crate::virtq::{Buf, Queue};

const VIRTIO_ID_BLOCK: u32 = 2;
const VIRTIO_BLK_F_SIZE_MAX: u64 = 1 << 1;
const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const QUEUE_SIZE: u32 = blk::RING_SIZE as u32;
/// Segments of a request, leaving room for data sharing the header and
/// status buffers.
const SEG_MAX: u32 = blk::MAX_SEGS as u32 - 2;
/// The largest segment the guest's DMA layer maps at once (swiotlb).
const SEG_SIZE_LIMIT: u32 = 256 << 10;

/// Largest segment for a guest whose I/O pool has `pool` bytes.  Every
/// request bounces through the pool, so one must never need more than a
/// part of it: a request that cannot fit even in an empty pool would be
/// retried forever.  seg_max * size_max is at most a quarter of the pool.
pub fn size_max(pool: u64) -> u32 {
    let per_seg = (pool / 4 / SEG_MAX as u64).min(SEG_SIZE_LIMIT as u64) as u32;
    (per_seg & !4095).max(4096)
}

#[derive(Clone, Copy)]
struct Inflight {
    status_gpa: u64,
    written: u32,
}

pub struct Blk {
    chan: *mut Channel,
    kick: sel4::cap::Notification,
    capacity: u64,
    read_only: bool,
    /// Logical block size of the disk: the guest aligns requests to it.
    block_size: u32,
    /// Largest data segment (see size_max).
    size_max: u32,
    /// Requests submitted to blkd, by chain head: at most one per head.
    inflight: [Option<Inflight>; QUEUE_SIZE as usize],
    /// Requests of this generation blkd has not completed yet.
    outstanding: usize,
    /// Bumped on reset, so completions of earlier requests are dropped.
    generation: u32,
    /// A chain popped from the queue that did not fit in the channel.
    stalled: Option<u16>,
}

impl Blk {
    /// The channel must be ready.  `pool` is the size of the guest's I/O
    /// pool.
    pub fn new(chan_va: usize, kick: sel4::cap::Notification, pool: u64) -> Self {
        let chan = chan_va as *mut Channel;
        let (capacity, read_only, block_size) = unsafe {
            fence(Ordering::Acquire);
            (
                addr_of!((*chan).capacity_sectors).read_volatile(),
                addr_of!((*chan).read_only).read_volatile() != 0,
                addr_of!((*chan).block_size).read_volatile().max(512),
            )
        };
        Blk {
            chan,
            kick,
            capacity,
            read_only,
            block_size,
            size_max: size_max(pool).max(block_size),
            inflight: [None; QUEUE_SIZE as usize],
            outstanding: 0,
            generation: 0,
            stalled: None,
        }
    }

    fn sq_full(&self) -> bool {
        unsafe {
            let head = addr_of!((*self.chan).sq.head).read_volatile();
            let tail = addr_of!((*self.chan).sq.tail).read_volatile();
            head.wrapping_sub(tail) as usize >= blk::RING_SIZE
        }
    }

    /// Complete a chain locally with `status`.
    fn finish(mem: &GuestMem, q: &mut Queue, head: u16, status_gpa: u64, status: u8, written: u32) {
        mem.write::<u8>(status_gpa, status);
        q.push(mem, head, written);
    }

    /// The response `id` of this generation arrived: the request it
    /// identifies, if it was in flight.
    fn retire(&mut self, id: u64) -> Option<(u16, Inflight)> {
        if (id >> 32) as u32 != self.generation {
            return None;
        }
        let h = id as u16;
        let inf = self.inflight.get_mut(h as usize).and_then(|i| i.take())?;
        self.outstanding -= 1;
        Some((h, inf))
    }

    /// Handle one chain.  Returns false if it must wait for channel space.
    fn submit(&mut self, mem: &GuestMem, q: &mut Queue, head: u16) -> bool {
        // The driver offered a chain blkd still owns.  Requests are known
        // by their head, so drop this one: the head returns to the driver
        // once, when the first request completes.  A correct driver never
        // does this.
        if self.inflight.get(head as usize).is_some_and(|i| i.is_some()) {
            return true;
        }
        let mut bufs = ArrayVec::<Buf, { QUEUE_SIZE as usize }>::new();
        let ok = q.chain(mem, head, &mut bufs).is_ok();
        self.submit_bufs(mem, q, head, ok, &bufs)
    }

    fn submit_bufs(&mut self, mem: &GuestMem, q: &mut Queue, head: u16, ok: bool, bufs: &[Buf]) -> bool {
        // Header first (16 bytes, readable), status byte last (writable).
        let last = bufs.last();
        let valid = ok
            && bufs.len() >= 2
            && !bufs[0].write
            && bufs[0].len >= 16
            && last.is_some_and(|b| b.write && b.len >= 1);
        if !valid {
            // Malformed: if there is a writable last byte, report an error
            // there; otherwise just return the chain.
            match last {
                Some(b) if ok && b.write && b.len >= 1 => {
                    Self::finish(mem, q, head, b.gpa + b.len as u64 - 1, VIRTIO_BLK_S_IOERR, 1)
                }
                _ => q.push(mem, head, 0),
            }
            return true;
        }
        let st = last.unwrap();
        let status_gpa = st.gpa + st.len as u64 - 1;
        let ty = mem.read::<u32>(bufs[0].gpa);
        let sector = mem.read::<u64>(bufs[0].gpa + 8);

        let mut req = blk::Req { id: 0, op: ty, nr_segs: 0, sector, segs: Default::default() };
        let mut data_len: u64 = 0;
        // Bytes the device writes, the status byte included (u64: a
        // hostile chain can describe more than 4 GiB).
        let mut written: u64 = 1;
        let mut seg = |gpa: u64, len: u32, write: bool, req: &mut blk::Req| -> bool {
            if len == 0 {
                return true;
            }
            if req.nr_segs as usize == blk::MAX_SEGS {
                return false;
            }
            req.segs[req.nr_segs as usize] = blk::Seg { gpa, len, flags: if write { blk::SEG_WRITE } else { 0 } };
            req.nr_segs += 1;
            data_len += len as u64;
            if write {
                written += len as u64;
            }
            true
        };
        let mut fits = true;
        if bufs[0].len > 16 {
            fits &= seg(bufs[0].gpa + 16, bufs[0].len - 16, false, &mut req);
        }
        for b in &bufs[1..bufs.len() - 1] {
            fits &= seg(b.gpa, b.len, b.write, &mut req);
        }
        if st.len > 1 {
            fits &= seg(st.gpa, st.len - 1, true, &mut req);
        }

        let status = match ty {
            blk::OP_READ | blk::OP_WRITE => {
                let dir_ok = req.segs[..req.nr_segs as usize]
                    .iter()
                    .all(|s| (s.flags & blk::SEG_WRITE != 0) == (ty == blk::OP_READ));
                let sectors = data_len / 512;
                if !fits || !dir_ok || data_len % 512 != 0 {
                    Some(VIRTIO_BLK_S_IOERR)
                } else if sector.checked_add(sectors).is_none_or(|end| end > self.capacity) {
                    Some(VIRTIO_BLK_S_IOERR)
                } else if ty == blk::OP_WRITE && self.read_only {
                    Some(VIRTIO_BLK_S_IOERR)
                } else {
                    None
                }
            }
            blk::OP_FLUSH => None,
            blk::OP_GET_ID => {
                if fits && req.nr_segs == 1 && req.segs[0].flags & blk::SEG_WRITE != 0 {
                    None
                } else {
                    Some(VIRTIO_BLK_S_IOERR)
                }
            }
            _ => Some(VIRTIO_BLK_S_UNSUPP),
        };
        if let Some(s) = status {
            Self::finish(mem, q, head, status_gpa, s, 1);
            return true;
        }
        if self.sq_full() {
            return false;
        }
        req.id = ((self.generation as u64) << 32) | head as u64;
        let written = u32::try_from(written).unwrap_or(u32::MAX);
        self.inflight[head as usize] = Some(Inflight { status_gpa, written });
        self.outstanding += 1;
        unsafe {
            let h = addr_of!((*self.chan).sq.head).read_volatile();
            addr_of_mut!((*self.chan).reqs[h as usize % blk::RING_SIZE]).write_volatile(req);
            fence(Ordering::Release);
            addr_of_mut!((*self.chan).sq.head).write_volatile(h.wrapping_add(1));
        }
        true
    }
}

impl Backend for Blk {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }

    fn features(&self) -> u64 {
        let mut f = VIRTIO_BLK_F_SIZE_MAX | VIRTIO_BLK_F_SEG_MAX | VIRTIO_BLK_F_BLK_SIZE | VIRTIO_BLK_F_FLUSH | VIRTIO_RING_F_INDIRECT_DESC;
        if self.read_only {
            f |= VIRTIO_BLK_F_RO;
        }
        f
    }

    fn num_queues(&self) -> usize {
        1
    }

    fn queue_max(&self, _q: usize) -> u32 {
        QUEUE_SIZE
    }

    fn init_config(&self, config: &mut [u8]) {
        config[0..8].copy_from_slice(&self.capacity.to_le_bytes());
        config[8..12].copy_from_slice(&self.size_max.to_le_bytes());
        config[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());

        config[20..24].copy_from_slice(&self.block_size.to_le_bytes());
    }

    fn kick(&mut self, mem: &GuestMem, queues: &mut [Queue], q: usize) -> bool {
        let queue = &mut queues[q];
        let mut returned = false;
        let mut submitted = false;
        loop {
            let head = match self.stalled.take() {
                Some(h) => h,
                None => match queue.pop(mem) {
                    Some(h) => h,
                    None => break,
                },
            };
            let used_before = queue_used_count(queue);
            if !self.submit(mem, queue, head) {
                self.stalled = Some(head);
                break;
            }
            if queue_used_count(queue) != used_before {
                returned = true;
            } else {
                submitted = true;
            }
        }
        if submitted {
            self.kick.signal();
        }
        returned
    }

    fn complete(&mut self, mem: &GuestMem, queues: &mut [Queue]) -> bool {
        let queue = &mut queues[0];
        let mut returned = false;
        unsafe {
            loop {
                let tail = addr_of!((*self.chan).cq.tail).read_volatile();
                let head = addr_of!((*self.chan).cq.head).read_volatile();
                if tail == head {
                    break;
                }
                fence(Ordering::Acquire);
                let resp = addr_of!((*self.chan).resps[tail as usize % blk::RING_SIZE]).read_volatile();
                addr_of_mut!((*self.chan).cq.tail).write_volatile(tail.wrapping_add(1));
                let Some((h, inf)) = self.retire(resp.id) else {
                    continue;
                };
                if !queue.ready {
                    continue;
                }
                let status = match resp.status {
                    blk::STATUS_OK => VIRTIO_BLK_S_OK,
                    blk::STATUS_UNSUPP => VIRTIO_BLK_S_UNSUPP,
                    _ => VIRTIO_BLK_S_IOERR,
                };
                let written = if status == VIRTIO_BLK_S_OK { inf.written } else { 1 };
                Self::finish(mem, queue, h, inf.status_gpa, status, written);
                returned = true;
            }
        }
        returned
    }

    fn pending(&self) -> bool {
        self.stalled.is_some()
            || unsafe { addr_of!((*self.chan).cq.tail).read_volatile() != addr_of!((*self.chan).cq.head).read_volatile() }
    }

    fn drain(&mut self) {
        self.stalled = None;
        while self.outstanding != 0 {
            unsafe {
                let tail = addr_of!((*self.chan).cq.tail).read_volatile();
                if tail == addr_of!((*self.chan).cq.head).read_volatile() {
                    // blkd runs at a higher priority, on its own core or
                    // as soon as we yield.
                    sel4::r#yield();
                    continue;
                }
                fence(Ordering::Acquire);
                let resp = addr_of!((*self.chan).resps[tail as usize % blk::RING_SIZE]).read_volatile();
                addr_of_mut!((*self.chan).cq.tail).write_volatile(tail.wrapping_add(1));
                self.retire(resp.id);
            }
        }
        // Requests completed from now on belong to the next generation.
        self.generation = self.generation.wrapping_add(1);
    }

    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.inflight = [None; QUEUE_SIZE as usize];
        self.outstanding = 0;
        self.stalled = None;
    }
}

fn queue_used_count(q: &Queue) -> u16 {
    q.used_idx()
}
