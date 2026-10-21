//! The host's free memory: power-of-two Untyped blocks, buddy style, and
//! the slots of memd's CNode that hold them.
//!
//! A request takes the smallest block that fits and splits it in halves
//! as needed, so large aligned allocations never waste memory on
//! alignment.  The Untyped of every split block is kept (with the halves
//! as its children), and deleting both halves makes it whole again: guest
//! RAM comes back in 2 MiB blocks, and without merging the free memory
//! would end up in 2 MiB pieces.  Blocks merge only when a larger one is
//! needed: the next retype of a merged parent makes seL4 clear all the
//! memory its children used, which for a large parent costs far more than
//! the request that split it.
//!
//! Blocks that stay free are reported to the hypervisor, if it takes free
//! page reports (`balloon`), so that it can drop their memory.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ops::Range;

use l4h_rt::{cap, host, sel4};
use sel4::{ObjectBlueprint, cap_type};

use crate::balloon::{self, Balloon};

pub type Slot = u64;

pub fn cnode() -> sel4::cap::CNode {
    cap::<cap_type::CNode>(host::SLOT_CNODE)
}

/// Slot `s` of memd's CNode, for CNode operations.
pub fn abs(s: Slot) -> sel4::AbsoluteCPtr {
    cnode().absolute_cptr(cap::<cap_type::Null>(s))
}

pub fn untyped(s: Slot) -> sel4::cap::Untyped {
    cap::<cap_type::Untyped>(s)
}

/// Free slots of memd's CNode, as coalesced ranges.
pub struct Slots {
    free: BTreeMap<Slot, Slot>,
}

impl Slots {
    pub fn new(range: Range<Slot>) -> Self {
        let mut free = BTreeMap::new();
        free.insert(range.start, range.end);
        Slots { free }
    }

    pub fn alloc(&mut self) -> Slot {
        self.alloc_range(1).start
    }

    /// `n` consecutive slots (the lowest range that fits).
    pub fn alloc_range(&mut self, n: u64) -> Range<Slot> {
        let (start, end) = self
            .free
            .iter()
            .find(|&(s, e)| e - s >= n)
            .map(|(&s, &e)| (s, e))
            .expect("memd: CNode full");
        self.free.remove(&start);
        if end - start > n {
            self.free.insert(start + n, end);
        }
        start..start + n
    }

    pub fn free(&mut self, s: Slot) {
        let (mut start, mut end) = (s, s + 1);
        if let Some((&ps, &pe)) = self.free.range(..=s).next_back() {
            assert!(pe <= s, "memd: slot {} freed twice", s);
            if pe == s {
                self.free.remove(&ps);
                start = ps;
            }
        }
        if let Some(ne) = self.free.remove(&end) {
            end = ne;
        }
        self.free.insert(start, end);
    }
}

/// What the hypervisor l4h runs in knows of a free block (balloon.rs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hint {
    /// Not reported; free since this tick count.
    Free(u64),
    /// In the report in flight: not to be used until the device returns it.
    Reporting,
    /// Reported: the hypervisor may have dropped the memory.
    Reported,
}

/// An untouched Untyped (no children) in a slot of ours.
struct FreeBlock {
    slot: Slot,
    paddr: usize,
    bits: usize,
    hint: Hint,
}

pub struct Mem {
    pub slots: Slots,
    blocks: Vec<FreeBlock>,
    /// Split blocks, (paddr, size bits) -> slot of their Untyped.
    parents: BTreeMap<(usize, usize), Slot>,
    pub balloon: Option<Balloon>,
}

impl Mem {
    pub fn new(slots: Slots) -> Self {
        Mem { slots, blocks: Vec::new(), parents: BTreeMap::new(), balloon: None }
    }

    /// Add the Untyped in `slot`, which has no children, to the free
    /// blocks.
    pub fn add(&mut self, slot: Slot, paddr: usize, bits: usize) {
        self.blocks.push(FreeBlock { slot, paddr, bits, hint: Hint::Free(host::ticks::now()) });
    }

    pub fn free_bytes(&self) -> usize {
        self.blocks.iter().map(|b| 1usize << b.bits).sum()
    }

    /// Free memory reported to the hypervisor, in bytes.
    pub fn reported_bytes(&self) -> usize {
        self.blocks.iter().filter(|b| b.hint == Hint::Reported).map(|b| 1usize << b.bits).sum()
    }

    /// Free blocks: (count, bytes of the largest).
    pub fn block_stats(&self) -> (usize, usize) {
        (self.blocks.len(), self.blocks.iter().map(|b| 1usize << b.bits).max().unwrap_or(0))
    }

    /// Whether a block of 2^bits bytes can be split off now, without
    /// merging or waiting for a report.
    fn has_block(&self, bits: usize) -> bool {
        self.blocks.iter().any(|b| b.bits >= bits && b.hint != Hint::Reporting)
    }

    /// An untouched Untyped of exactly 2^bits bytes: (slot, paddr).  It is
    /// the caller's until it gives it back with `add`.
    pub fn take(&mut self, bits: usize) -> Option<(Slot, usize)> {
        if !self.has_block(bits) {
            self.finish_report(true);
        }
        if !self.has_block(bits) {
            self.merge_up_to(bits);
        }
        let (i, _) = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.bits >= bits && b.hint != Hint::Reporting)
            .min_by_key(|(_, b)| (b.bits, b.paddr))?;
        let mut b = self.blocks.swap_remove(i);
        while b.bits > bits {
            let half = b.bits - 1;
            let s = self.slots.alloc_range(2);
            untyped(b.slot)
                .untyped_retype(&ObjectBlueprint::Untyped { size_bits: half }, &cnode().absolute_cptr_for_self(), s.start as usize, 2)
                .expect("memd: split block");
            // The parent is kept, to merge the halves again when both
            // are free.
            self.parents.insert((b.paddr, b.bits), b.slot);
            self.blocks.push(FreeBlock { slot: s.start + 1, paddr: b.paddr + (1 << half), bits: half, hint: b.hint });
            b = FreeBlock { slot: s.start, paddr: b.paddr, bits: half, hint: b.hint };
        }
        Some((b.slot, b.paddr))
    }

    /// Merge free buddies, smallest first, into blocks of up to 2^bits
    /// bytes, so that a request for 2^bits can be met.
    fn merge_up_to(&mut self, bits: usize) {
        loop {
            let mut merged = false;
            let mut i = 0;
            while i < self.blocks.len() {
                let (paddr, b) = (self.blocks[i].paddr, self.blocks[i].bits);
                let parent = (paddr & !(1 << b), b + 1);
                let buddy = paddr ^ (1 << b);
                let pslot = self.parents.get(&parent).copied();
                let j = self.blocks.iter().position(|x| x.paddr == buddy && x.bits == b);
                let (Some(pslot), Some(j), true) = (pslot, j, b < bits) else {
                    i += 1;
                    continue;
                };
                if self.blocks[i].hint == Hint::Reporting || self.blocks[j].hint == Hint::Reporting {
                    i += 1;
                    continue;
                }
                // Neither half has children: deleting both leaves the
                // parent without children, so its next retype starts
                // afresh (and clears what the halves used).
                let (x, y) = (self.blocks.swap_remove(i.max(j)), self.blocks.swap_remove(i.min(j)));
                let hint = if x.hint == Hint::Reported && y.hint == Hint::Reported {
                    Hint::Reported
                } else {
                    Hint::Free(host::ticks::now())
                };
                for half in [x.slot, y.slot] {
                    abs(half).delete().expect("memd: delete merged half");
                    self.slots.free(half);
                }
                self.parents.remove(&parent);
                self.blocks.push(FreeBlock { slot: pslot, paddr: parent.0, bits: parent.1, hint });
                merged = true;
            }
            if !merged || self.has_block(bits) {
                return;
            }
        }
    }

    /// Report the blocks that have been free for `delay` ticks to the
    /// hypervisor, as many as one report holds, unless a report is in
    /// flight.  Called periodically.
    pub fn report_free(&mut self, now: u64, delay: u64) {
        self.finish_report(false);
        let Some(dev) = &mut self.balloon else { return };
        if dev.busy() {
            return;
        }
        let mut ranges = Vec::new();
        for b in &mut self.blocks {
            let Hint::Free(since) = b.hint else { continue };
            if b.bits < balloon::MIN_BITS || now.saturating_sub(since) < delay {
                continue;
            }
            let piece = 1usize << b.bits.min(balloon::MAX_DESC_BITS);
            let n = (1usize << b.bits) / piece;
            if ranges.len() + n > dev.capacity() {
                continue;
            }
            ranges.extend((0..n).map(|k| (b.paddr + k * piece, piece)));
            b.hint = Hint::Reporting;
        }
        if !ranges.is_empty() {
            dev.report(&ranges);
        }
    }

    /// Take back the blocks of the report in flight once the device is
    /// done with it; `wait` waits for that.
    fn finish_report(&mut self, wait: bool) {
        let Some(dev) = &mut self.balloon else { return };
        if !dev.busy() {
            return;
        }
        if !dev.done(wait) {
            if wait {
                sel4::debug_println!("memd: balloon: the device holds a free page report too long");
            }
            return;
        }
        let mut bytes = 0;
        for b in &mut self.blocks {
            if b.hint == Hint::Reporting {
                b.hint = Hint::Reported;
                bytes += 1usize << b.bits;
            }
        }
        l4h_rt::info!(
            "memd: balloon: reported {} MiB, {} of {} MiB free reported",
            bytes >> 20, self.reported_bytes() >> 20, self.free_bytes() >> 20
        );
    }
}
