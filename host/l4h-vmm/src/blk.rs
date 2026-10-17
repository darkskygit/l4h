//! virtio-blk device backed by a blkd channel.  Data buffers are passed
//! to blkd by guest-physical address; nothing is copied here.

use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_rt::host::blk::{self, Channel};
use l4h_rt::sel4;

use crate::dev::{Backend, VIRTIO_RING_F_INDIRECT_DESC};
use crate::guest::GuestMem;
use crate::virtq::{Buf, Queue};

const VIRTIO_ID_BLOCK: u32 = 2;
const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const QUEUE_SIZE: u32 = blk::RING_SIZE as u32;

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
    /// Requests submitted to blkd, by chain head.
    inflight: [Option<Inflight>; QUEUE_SIZE as usize],
    /// Bumped on reset, so completions of earlier requests are dropped.
    generation: u32,
    /// A chain popped from the queue that did not fit in the channel.
    stalled: Option<u16>,
    bufs: Vec<Buf>,
}

impl Blk {
    /// The channel must be ready.
    pub fn new(chan_va: usize, kick: sel4::cap::Notification) -> Self {
        let chan = chan_va as *mut Channel;
        let (capacity, read_only) = unsafe {
            fence(Ordering::Acquire);
            (addr_of!((*chan).capacity_sectors).read_volatile(), addr_of!((*chan).read_only).read_volatile() != 0)
        };
        Blk {
            chan,
            kick,
            capacity,
            read_only,
            inflight: [None; QUEUE_SIZE as usize],
            generation: 0,
            stalled: None,
            bufs: Vec::new(),
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

    /// Handle one chain.  Returns false if it must wait for channel space.
    fn submit(&mut self, mem: &GuestMem, q: &mut Queue, head: u16) -> bool {
        let mut bufs = core::mem::take(&mut self.bufs);
        let ok = q.chain(mem, head, &mut bufs).is_ok();
        let r = self.submit_bufs(mem, q, head, ok, &bufs);
        self.bufs = bufs;
        r
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
        let mut written: u32 = 1;
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
                written += len;
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
        self.inflight[head as usize] = Some(Inflight { status_gpa, written });
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
        let mut f = VIRTIO_BLK_F_SEG_MAX | VIRTIO_BLK_F_BLK_SIZE | VIRTIO_BLK_F_FLUSH | VIRTIO_RING_F_INDIRECT_DESC;
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
        // seg_max: leave room for data sharing the header/status buffers.
        config[12..16].copy_from_slice(&(blk::MAX_SEGS as u32 - 2).to_le_bytes());
        config[20..24].copy_from_slice(&512u32.to_le_bytes());
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
                if (resp.id >> 32) as u32 != self.generation || !queue.ready {
                    continue;
                }
                let h = resp.id as u16;
                let Some(inf) = self.inflight.get_mut(h as usize).and_then(|i| i.take()) else {
                    continue;
                };
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

    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.inflight = [None; QUEUE_SIZE as usize];
        self.stalled = None;
    }
}

fn queue_used_count(q: &Queue) -> u16 {
    q.used_idx()
}
