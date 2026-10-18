//! Untyped memory and CSpace slot management for the root task.
//!
//! All kernel objects are carved out of Untyped capabilities.  We keep our
//! own copy of each Untyped's watermark so that we know the physical address
//! of every child we create (seL4 aligns each retyped object to its size,
//! starting at the current watermark).  Guest RAM relies on this to obtain
//! physically contiguous chunks.

use alloc::collections::{BTreeMap, BTreeSet};

use alloc::vec::Vec;
use core::ops::Range;

use sel4::init_thread::slot;
use sel4::{ObjectBlueprint, cap};

pub type Slot = usize;

/// Allocator for slots in the root task's own CNode.
///
/// Free slots are kept as coalesced ranges, so that ranges freed by
/// destroyed guests (vNIC ports, disk DMA buffers) can be allocated again
/// as ranges.
///
/// While tracking is on, every slot allocated (and not freed again) is
/// recorded, so that the slots holding a guest's capabilities can be
/// cleaned up and reused when the guest is destroyed.
pub struct Slots {
    /// Free ranges: start -> end (exclusive).
    free: BTreeMap<Slot, Slot>,
    count: usize,
    track: Option<BTreeSet<Slot>>,
}

impl Slots {
    pub fn new(range: Range<usize>) -> Self {
        let count = range.len();
        let mut free = BTreeMap::new();
        if count > 0 {
            free.insert(range.start, range.end);
        }
        Self { free, count, track: None }
    }

    pub fn alloc(&mut self) -> Slot {
        self.alloc_range(1).start
    }

    /// Allocate `n` consecutive slots (the lowest range that fits).
    pub fn alloc_range(&mut self, n: usize) -> Range<Slot> {
        let (start, end) = self
            .free
            .iter()
            .find(|&(s, e)| e - s >= n)
            .map(|(&s, &e)| (s, e))
            .expect("root CNode full");
        self.free.remove(&start);
        if end - start > n {
            self.free.insert(start + n, end);
        }
        self.count -= n;
        if let Some(t) = &mut self.track {
            t.extend(start..start + n);
        }
        start..start + n
    }

    pub fn free(&mut self, s: Slot) {
        if let Some(t) = &mut self.track {
            t.remove(&s);
        }
        let (mut start, mut end) = (s, s + 1);
        if let Some((&ps, &pe)) = self.free.range(..=s).next_back() {
            assert!(pe <= s, "slot {} freed twice", s);
            if pe == s {
                self.free.remove(&ps);
                start = ps;
            }
        }
        if let Some(ne) = self.free.remove(&end) {
            end = ne;
        }
        self.free.insert(start, end);
        self.count += 1;
    }

    /// Slots that can still be allocated.
    pub fn available(&self) -> usize {
        self.count
    }

    /// The longest range `alloc_range` can return.
    pub fn largest_range(&self) -> usize {
        self.free.iter().map(|(s, e)| e - s).max().unwrap_or(0)
    }

    /// Start recording allocations into `set`.
    pub fn track(&mut self, set: BTreeSet<Slot>) {
        assert!(self.track.is_none(), "nested slot tracking");
        self.track = Some(set);
    }

    /// Stop recording; returns the recorded set.
    pub fn untrack(&mut self) -> BTreeSet<Slot> {
        self.track.take().unwrap_or_default()
    }

    /// Run `f` with tracking suspended (for objects that outlive a guest).
    pub fn global<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let t = self.track.take();
        let r = f(self);
        self.track = t;
        r
    }
}

/// One Untyped capability and our view of its watermark.
pub struct UtRegion {
    pub cap: cap::Untyped,
    pub paddr: usize,
    pub size_bits: usize,
    used: usize,
}

impl UtRegion {
    /// A region whose capability has no children.
    pub fn fresh(cap: cap::Untyped, paddr: usize, size_bits: usize) -> Self {
        UtRegion { cap, paddr, size_bits, used: 0 }
    }

    fn size(&self) -> usize {
        1 << self.size_bits
    }

    /// Offset at which `count` objects of 2^bits bytes would be placed.
    fn fit(&self, bits: usize, count: usize) -> Option<usize> {
        let align = 1usize << bits;
        let off = (self.used + align - 1) & !(align - 1);
        (bits <= self.size_bits && off + (count << bits) <= self.size()).then_some(off)
    }
}

pub fn root_cnode_dest() -> sel4::AbsoluteCPtr {
    slot::CNODE.cap().absolute_cptr_for_self()
}

/// Where a retype should place its result.
#[derive(Clone, Copy)]
pub struct Dest {
    /// CNode the objects are placed in (as seen from the root CSpace).
    pub cnode: sel4::AbsoluteCPtr,
    /// First slot index within that CNode.
    pub offset: usize,
}

impl Dest {
    pub fn root(slot: Slot) -> Self {
        Dest { cnode: root_cnode_dest(), offset: slot }
    }
}

/// A pool of Untyped regions that objects are allocated from.  The global
/// pool owns the boot Untypeds; per-guest arenas own regions split off it so
/// that a guest can be torn down by revoking a handful of capabilities.
pub struct UtPool {
    pub regions: Vec<UtRegion>,
}

impl UtPool {
    pub fn empty() -> Self {
        Self { regions: Vec::new() }
    }

    pub fn from_bootinfo(bootinfo: &sel4::BootInfo) -> Self {
        let mut regions = Vec::new();
        for (i, desc) in bootinfo.untyped_list().iter().enumerate() {
            if desc.is_device() {
                continue;
            }
            regions.push(UtRegion {
                cap: bootinfo.untyped().index(i).cap(),
                paddr: desc.paddr(),
                size_bits: desc.size_bits(),
                used: 0,
            });
        }
        // Prefer small regions for small objects so big ones stay intact.
        regions.sort_by_key(|r| r.size_bits);
        Self { regions }
    }

    pub fn free_bytes(&self) -> usize {
        self.regions.iter().map(|r| r.size() - r.used).sum()
    }

    /// Retype `count` objects of `bp` into `dest`.  All objects come from a
    /// single region, so they are physically contiguous and each is aligned
    /// to its size.  Returns the physical address of the first one.
    pub fn retype(&mut self, bp: &ObjectBlueprint, count: usize, dest: Dest) -> Option<usize> {
        let bits = bp.physical_size_bits();
        // Take the region where alignment wastes least (an aligned
        // allocation skips the memory between the watermark and its start,
        // for good); among equals, the smallest region.
        let idx = self
            .regions
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.fit(bits, count).map(|off| (off - r.used, r.size_bits, i)))
            .min()
            .map(|(_, _, i)| i)?;
        let r = &mut self.regions[idx];
        let off = r.fit(bits, count)?;
        // The kernel caps fan-out per call; split large requests.
        let mut done = 0;
        while done < count {
            let n = (count - done).min(crate::RETYPE_FAN_OUT);
            r.cap
                .untyped_retype(bp, &dest.cnode, dest.offset + done, n)
                .unwrap_or_else(|e| panic!("retype {:?} x{} failed: {:?}", bp.ty(), n, e));
            done += n;
        }
        r.used = off + (count << bits);
        Some(r.paddr + off)
    }

    /// Split off a child Untyped of 2^bits bytes into root slot `slot` and
    /// return it as a region (owned by the caller).
    pub fn split(&mut self, bits: usize, slot: Slot) -> Option<UtRegion> {
        let bp = ObjectBlueprint::Untyped { size_bits: bits };
        let paddr = self.retype(&bp, 1, Dest::root(slot))?;
        Some(UtRegion {
            cap: sel4::init_thread::Slot::<sel4::cap_type::Untyped>::from_index(slot).cap(),
            paddr,
            size_bits: bits,
            used: 0,
        })
    }
}

/// Global allocation state of the root task.
///
/// Free memory is kept as untouched power-of-two Untyped *blocks* (buddy
/// style, without coalescing): a request takes the smallest block that
/// fits and splits it in halves as needed, so large aligned allocations
/// (guest RAM) never waste memory on alignment.  Memory reclaimed from
/// guests returns as blocks.  Small global objects come from `pool`, which
/// is refilled with blocks.
pub struct Alloc {
    pub slots: Slots,
    /// Small objects of the root task itself.
    pub pool: UtPool,
    blocks: Vec<UtRegion>,
}

const SMALL_POOL_BITS: usize = 24;

fn untyped_at(s: Slot) -> cap::Untyped {
    sel4::init_thread::Slot::<sel4::cap_type::Untyped>::from_index(s).cap()
}

impl Alloc {
    pub fn new(slots: Slots, bootinfo: &sel4::BootInfo) -> Self {
        let mut a = Alloc { slots, pool: UtPool::empty(), blocks: UtPool::from_bootinfo(bootinfo).regions };
        a.refill();
        a
    }

    /// Keep the small-object pool stocked.
    pub fn refill(&mut self) {
        if self.pool.free_bytes() >= 1 << (SMALL_POOL_BITS - 1) {
            return;
        }
        let s = self.slots.global(|s| s.alloc());
        if let Some(r) = self.split(SMALL_POOL_BITS, s) {
            self.pool.regions.push(r);
            self.pool.regions.sort_by_key(|r| r.size_bits);
        }
    }

    pub fn free_bytes(&self) -> usize {
        self.blocks.iter().map(|b| 1usize << b.size_bits).sum::<usize>() + self.pool.free_bytes()
    }

    /// Largest block available.
    pub fn largest_block_bits(&self) -> Option<usize> {
        self.blocks.iter().map(|b| b.size_bits).max()
    }

    /// An untouched Untyped of exactly 2^bits bytes, in root slot `slot`.
    pub fn split(&mut self, bits: usize, slot: Slot) -> Option<UtRegion> {
        let (i, _) = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.size_bits >= bits)
            .min_by_key(|(_, b)| (b.size_bits, b.paddr))?;
        let mut b = self.blocks.swap_remove(i);
        while b.size_bits > bits {
            let half = b.size_bits - 1;
            let s = self.slots.global(|s| s.alloc_range(2));
            b.cap
                .untyped_retype(&ObjectBlueprint::Untyped { size_bits: half }, &root_cnode_dest(), s.start, 2)
                .expect("split block");
            // The parent capability is not needed any more: the halves
            // stay valid, and its memory is never retyped again.
            let parent = b.cap.bits() as Slot;
            slot::CNODE.cap().absolute_cptr(b.cap).delete().expect("delete split parent");
            self.slots.global(|sl| sl.free(parent));
            self.blocks.push(UtRegion::fresh(untyped_at(s.start + 1), b.paddr + (1 << half), half));
            b = UtRegion::fresh(untyped_at(s.start), b.paddr, half);
        }
        let dst = slot::CNODE.cap().absolute_cptr(untyped_at(slot));
        dst.move_(&slot::CNODE.cap().absolute_cptr(b.cap)).expect("move block");
        self.slots.global(|s| s.free(b.cap.bits() as Slot));
        Some(UtRegion::fresh(untyped_at(slot), b.paddr, bits))
    }

    /// Allocate a single object into a fresh root slot.
    pub fn object(&mut self, bp: ObjectBlueprint) -> Slot {
        self.refill();
        let s = self.slots.alloc();
        self.pool
            .retype(&bp, 1, Dest::root(s))
            .unwrap_or_else(|| panic!("out of memory allocating {:?}", bp.ty()));
        s
    }

    pub fn object_cap<T: sel4::CapType>(&mut self, bp: ObjectBlueprint) -> sel4::Cap<T> {
        let s = self.object(bp);
        sel4::init_thread::Slot::<T>::from_index(s).cap()
    }

    /// Destroy every object of `arena` and return its memory as blocks.
    /// The Untyped capabilities move to fresh slots, leaving the owner's
    /// slots empty.
    pub fn reclaim(&mut self, arena: Arena) {
        for (slot, paddr, bits) in arena.owned {
            let cap = untyped_at(slot);
            slot::CNODE.cap().absolute_cptr(cap).revoke().expect("revoke arena untyped");
            let s = self.slots.global(|s| s.alloc());
            slot::CNODE.cap().absolute_cptr(untyped_at(s)).move_(&slot::CNODE.cap().absolute_cptr(cap)).expect("move block");
            self.blocks.push(UtRegion::fresh(untyped_at(s), paddr, bits));
        }
    }

    /// Undo a partly built arena: reclaim its memory and free its slots.
    fn release(&mut self, arena: Arena) {
        let slots: Vec<Slot> = arena.owned.iter().map(|o| o.0).collect();
        self.reclaim(arena);
        for s in slots {
            self.slots.free(s);
        }
    }

    /// Carve a per-guest arena of at least `bytes` out of the free blocks.
    pub fn arena(&mut self, bytes: usize) -> Option<Arena> {
        let mut arena = Arena { pool: UtPool::empty(), owned: Vec::new() };
        let mut remaining = bytes;
        while remaining > 0 {
            let Some(max_bits) = self.largest_block_bits() else {
                self.release(arena);
                return None;
            };
            // Binary decomposition: the largest power of two not above
            // what is left, and a rounded-up tail below 1 MiB.
            let want_bits = if remaining <= 1 << 20 {
                remaining.max(4096).next_power_of_two().trailing_zeros() as usize
            } else {
                (usize::BITS - 1 - remaining.leading_zeros()) as usize
            };
            let bits = want_bits.min(max_bits);
            let slot = self.slots.alloc();
            let Some(region) = self.split(bits, slot) else {
                self.slots.free(slot);
                self.release(arena);
                return None;
            };
            arena.owned.push((slot, region.paddr, bits));
            remaining = remaining.saturating_sub(1 << bits);
            arena.pool.regions.push(region);
        }
        arena.pool.regions.sort_by_key(|r| r.size_bits);
        Some(arena)
    }
}

/// Untyped memory dedicated to one guest.
pub struct Arena {
    pub pool: UtPool,
    /// Untyped capabilities owned by the guest (slot, paddr, size bits):
    /// the arena's own regions and others (RAM chunks, quota).  Revoking
    /// these destroys every object created from them.
    pub owned: Vec<(Slot, usize, usize)>,
}

impl Arena {
    pub fn own(&mut self, slot: Slot, paddr: usize, bits: usize) {
        self.owned.push((slot, paddr, bits));
    }

    pub fn object(&mut self, slots: &mut Slots, bp: ObjectBlueprint) -> Slot {
        let s = slots.alloc();
        self.pool
            .retype(&bp, 1, Dest::root(s))
            .unwrap_or_else(|| panic!("guest arena exhausted allocating {:?}", bp.ty()));
        s
    }

    pub fn object_cap<T: sel4::CapType>(&mut self, slots: &mut Slots, bp: ObjectBlueprint) -> sel4::Cap<T> {
        let s = self.object(slots, bp);
        sel4::init_thread::Slot::<T>::from_index(s).cap()
    }
}

/// Something objects can be allocated from: the global pool or an arena,
/// together with the root slot allocator.
pub struct ObjCtx<'a> {
    pub slots: &'a mut Slots,
    pub pool: &'a mut UtPool,
}

impl ObjCtx<'_> {
    pub fn object(&mut self, bp: ObjectBlueprint) -> Slot {
        let s = self.slots.alloc();
        self.pool
            .retype(&bp, 1, Dest::root(s))
            .unwrap_or_else(|| {
                for r in &self.pool.regions {
                    sel4::debug_println!("  region paddr={:#x} bits={} used={:#x}", r.paddr, r.size_bits, r.used);
                }
                panic!("out of memory allocating {:?} ({} bits)", bp.ty(), bp.physical_size_bits())
            });
        s
    }

    pub fn cap<T: sel4::CapType>(&mut self, bp: ObjectBlueprint) -> sel4::Cap<T> {
        let s = self.object(bp);
        sel4::init_thread::Slot::<T>::from_index(s).cap()
    }
}
