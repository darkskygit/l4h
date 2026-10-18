//! virtio-blk queue reset against a guest that lists chain heads at random,
//! including heads already in flight: when `drain` returns, blkd must have
//! completed every request it was given (it may still DMA into the
//! buffers of any other), and that no chain with a buffer outside the
//! guest's I/O pool ever reaches blkd.

use std::cell::RefCell;

use crate::Rng;
use crate::blk::Blk;
use crate::dev::Backend;
use crate::guest::GuestMem;
use crate::host::blk::{self as chan, Channel, Resp};
use crate::virtq::Queue;

/// The fake blkd: the channel and the requests it took but has not
/// completed (ids).
struct Blkd {
    chan: *mut Channel,
    pending: Vec<u64>,
    rng: Rng,
    /// Chain heads with a buffer outside the I/O pool: the VMM must refuse
    /// them.
    bad: u64,
}

thread_local! {
    static BLKD: RefCell<Option<Blkd>> = const { RefCell::new(None) };
}

impl Blkd {
    /// Take every submitted request.
    fn take(&mut self) {
        unsafe {
            let c = &mut *self.chan;
            while c.sq.tail != c.sq.head {
                let id = c.reqs[c.sq.tail as usize % chan::RING_SIZE].id;
                assert!(self.bad & (1 << (id as u16)) == 0, "chain {} outside the I/O pool reached blkd", id as u16);
                self.pending.push(id);
                c.sq.tail = c.sq.tail.wrapping_add(1);
            }
        }
    }

    /// Complete one pending request, in any order.  The completion ring
    /// has room for every request the VMM may have in flight.
    fn complete_one(&mut self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let i = self.rng.below(self.pending.len() as u64) as usize;
        let id = self.pending.swap_remove(i);
        unsafe {
            let c = &mut *self.chan;
            assert!(c.cq.head.wrapping_sub(c.cq.tail) < chan::RING_SIZE as u32, "completion ring overflows");
            c.resps[c.cq.head as usize % chan::RING_SIZE] = Resp { id, status: chan::STATUS_OK, reserved: 0 };
            c.cq.head = c.cq.head.wrapping_add(1);
        }
        true
    }
}

/// blkd runs while the VMM yields in `drain`.
pub fn blkd_run() {
    BLKD.with_borrow_mut(|b| {
        let b = b.as_mut().unwrap();
        b.take();
        assert!(b.complete_one(), "drain waits for requests blkd does not have");
    });
}

const NUM: u16 = 128;
const CHAINS: u16 = 64;
/// The I/O pool, above the guest's RAM.
const POOL: u64 = 0x4020_0000;
const DESC: u64 = POOL;
const AVAIL: u64 = POOL + 0x1000;
const USED: u64 = POOL + 0x2000;
const HDR: u64 = POOL + 0x3000;
const STATUS: u64 = POOL + 0x4000;

/// Returns the number of requests drained and of chains refused because a
/// buffer was outside the pool.
pub fn drain_check(rng: &mut Rng, iterations: u64) -> (u64, u64) {
    let (mut drained, mut refused) = (0, 0);
    for _ in 0..iterations {
        let mut ram = vec![0u64; 0x8000 / 8];
        let mem = GuestMem { base: ram.as_mut_ptr() as usize, gpa: POOL, size: 0x8000 };
        // Some chains have their status byte outside the pool: in guest
        // RAM below it, or just past its end.
        let bad: u64 = rng.next() & rng.next();
        // Chain h: a FLUSH header (descriptor h), then the status byte
        // (descriptor h + CHAINS).
        for h in 0..CHAINS as u64 {
            let (d, s) = (DESC + 16 * h, DESC + 16 * (h + CHAINS as u64));
            mem.write::<u64>(d, HDR + 16 * h);
            mem.write::<u32>(d + 8, 16);
            mem.write::<u16>(d + 12, crate::virtq::VIRTQ_DESC_F_NEXT);
            mem.write::<u16>(d + 14, h as u16 + CHAINS);
            mem.write::<u32>(HDR + 16 * h, chan::OP_FLUSH);
            let status = match (bad >> h & 1, h % 2) {
                (0, _) => STATUS + h,
                (_, 0) => POOL - 0x1000 + h,
                _ => POOL + 0x8000 + h,
            };
            mem.write::<u64>(s, status);
            mem.write::<u32>(s + 8, 1);
            mem.write::<u16>(s + 12, crate::virtq::VIRTQ_DESC_F_WRITE);
        }
        let mut channel: Box<Channel> = unsafe { Box::new_zeroed().assume_init() };
        channel.capacity_sectors = 1 << 20;
        channel.block_size = 512;
        let chan_ptr: *mut Channel = &mut *channel;
        let seed = rng.next() | 1;
        BLKD.with_borrow_mut(|b| *b = Some(Blkd { chan: chan_ptr, pending: Vec::new(), rng: Rng(seed), bad }));
        let mut blk = Blk::new(chan_ptr as usize, crate::sel4::cap::Notification, 0x8000);
        let mut queues = [Queue::default()];
        assert!(queues[0].setup(&mem, NUM as u32, NUM as u32, DESC, AVAIL, USED));
        let mut avail_idx = 0u16;
        for _ in 0..rng.below(8) + 1 {
            // The guest offers some chains: mostly fresh ones, sometimes
            // one that is still in flight.
            for _ in 0..rng.below(CHAINS as u64) {
                let h = rng.below(CHAINS as u64) as u16;
                refused += bad >> h & 1;
                mem.write::<u16>(AVAIL + 4 + 2 * (avail_idx % NUM) as u64, h);
                avail_idx = avail_idx.wrapping_add(1);
            }
            mem.write::<u16>(AVAIL + 2, avail_idx);
            blk.kick(&mem, &mut queues, 0);
            // blkd completes some of what it has.
            BLKD.with_borrow_mut(|b| {
                let b = b.as_mut().unwrap();
                b.take();
                for _ in 0..rng.below(b.pending.len() as u64 + 1) {
                    b.complete_one();
                }
            });
            if rng.below(2) == 0 {
                blk.complete(&mem, &mut queues);
            }
        }
        blk.drain();
        BLKD.with_borrow_mut(|b| {
            let b = b.as_mut().unwrap();
            b.take();
            assert!(b.pending.is_empty(), "drain returned with {} requests in blkd", b.pending.len());
        });
        drained += channel.cq.tail as u64;
        blk.reset();
        queues[0].reset();
        BLKD.with_borrow_mut(|b| *b = None);
        drop(ram);
    }
    (drained, refused)
}

