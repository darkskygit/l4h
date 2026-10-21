//! Untyped memory and CSpace slot management for the root task.
//!
//! All kernel objects are carved out of Untyped capabilities.  We keep our
//! own copy of each Untyped's watermark so that we know the physical address
//! of every child we create (seL4 aligns each retyped object to its size,
//! starting at the current watermark).  I/O pools rely on this to obtain
//! physically contiguous chunks.

use alloc::collections::{BTreeMap, BTreeSet};

use alloc::vec::Vec;
use core::ops::Range;

use sel4::init_thread::slot;
use l4h_host::memd::BLOCK_BITS;
use sel4::{ObjectBlueprint, cap};

use crate::memd::MemdLink;

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

impl Alloc {
    /// `Slots::global` for code that needs the whole allocator.
    pub fn global<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let t = self.slots.track.take();
        let r = f(self);
        self.slots.track = t;
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
/// Free memory is kept as untouched power-of-two Untyped *blocks*, buddy
/// style: a request takes the smallest block that fits and splits it in
/// halves as needed, so large aligned allocations never waste memory on
/// alignment.  The Untyped of every split block is kept (with the halves
/// as its children), and deleting both halves makes it whole.  Small
/// global objects come from `pool`, which is refilled with blocks.
///
/// At boot the blocks are the boot Untypeds.  Once l4h-memd runs, they all
/// go to it (`hand_over`), and the blocks here are a cache: a request that
/// they cannot meet takes a *grant* from memd (GRANT_BITS, or more for a
/// larger request), and grants that are whole and free again go back to
/// it (`trim`) beyond the LOCAL_KEEP bytes kept for the next requests.
/// Guest RAM does not come from here at all (memd.rs).
pub struct Alloc {
    pub slots: Slots,
    /// Small objects of the root task itself.
    pub pool: UtPool,
    blocks: Vec<UtRegion>,
    /// Split blocks, (paddr, size bits) -> slot of their Untyped.
    parents: BTreeMap<(usize, usize), Slot>,
    pub memd: Option<MemdLink>,
    /// Grants from memd that we hold, whole or split: (paddr, size bits).
    grants: BTreeSet<(usize, usize)>,
}

const SMALL_POOL_BITS: usize = 24;
/// Smallest grant memd is asked for.
const GRANT_BITS: usize = 24;
/// Free grants kept beyond what is in use.
const LOCAL_KEEP: usize = 32 << 20;

fn untyped_at(s: Slot) -> cap::Untyped {
    sel4::init_thread::Slot::<sel4::cap_type::Untyped>::from_index(s).cap()
}

impl Alloc {
    pub fn new(slots: Slots, bootinfo: &sel4::BootInfo) -> Self {
        let blocks = UtPool::from_bootinfo(bootinfo).regions;
        let mut a = Alloc { slots, pool: UtPool::empty(), blocks, parents: BTreeMap::new(), memd: None, grants: BTreeSet::new() };
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

    fn local_bytes(&self) -> usize {
        self.blocks.iter().map(|b| 1usize << b.size_bits).sum()
    }

    /// Free memory, here and in memd.
    pub fn free_bytes(&self) -> usize {
        self.local_bytes() + self.pool.free_bytes() + self.memd.map_or(0, |m| m.status().free_bytes as usize)
    }

    /// Free memory reported to the hypervisor, in bytes (memd).
    pub fn reported_bytes(&self) -> usize {
        self.memd.map_or(0, |m| m.status().reported_bytes as usize)
    }

    /// Largest block available, here or in memd.
    pub fn largest_block_bits(&self) -> Option<usize> {
        let memd = self.memd.map(|m| m.status().largest_block as usize).filter(|&b| b != 0).map(|b| b.trailing_zeros() as usize);
        self.blocks.iter().map(|b| b.size_bits).max().max(memd)
    }

    /// Whether a block of 2^bits bytes can be split off here.
    fn has_block(&self, bits: usize) -> bool {
        self.blocks.iter().any(|b| b.size_bits >= bits)
    }

    /// Take a grant from memd for a request of 2^bits bytes: GRANT_BITS,
    /// or exactly what is asked if memd has no larger block.
    fn grant(&mut self, bits: usize) -> bool {
        let Some(memd) = self.memd else { return false };
        for gbits in [bits.max(GRANT_BITS), bits.max(BLOCK_BITS)] {
            let s = self.slots.global(|s| s.alloc());
            if let Some(paddr) = memd.alloc(gbits, s) {
                self.blocks.push(UtRegion::fresh(untyped_at(s), paddr, gbits));
                self.grants.insert((paddr, gbits));
                return true;
            }
            self.slots.global(|sl| sl.free(s));
        }
        false
    }

    /// An untouched Untyped of exactly 2^bits bytes, in root slot `slot`.
    pub fn split(&mut self, bits: usize, slot: Slot) -> Option<UtRegion> {
        if !self.has_block(bits) {
            self.merge_up_to(bits);
        }
        if !self.has_block(bits) && !self.grant(bits) {
            return None;
        }
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
            // The parent is kept, to merge the halves again when both
            // are free.
            self.parents.insert((b.paddr, b.size_bits), b.cap.bits() as Slot);
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
        self.reclaim_pinning(arena, |_| false);
    }

    /// `reclaim`, except that the Untypeds at a physical address for which
    /// `pin` is true never return to the free pool: a device that could not
    /// be stopped may still write to them.  Their objects are destroyed all
    /// the same.  Returns the bytes kept out of use.
    pub fn reclaim_pinning(&mut self, arena: Arena, pin: impl Fn(usize) -> bool) -> usize {
        let mut pinned = 0;
        for (slot, paddr, bits) in arena.owned {
            let cap = untyped_at(slot);
            slot::CNODE.cap().absolute_cptr(cap).revoke().expect("revoke arena untyped");
            if pin(paddr) {
                pinned += 1 << bits;
                continue;
            }
            let s = self.slots.global(|s| s.alloc());
            slot::CNODE.cap().absolute_cptr(untyped_at(s)).move_(&slot::CNODE.cap().absolute_cptr(cap)).expect("move block");
            self.blocks.push(UtRegion::fresh(untyped_at(s), paddr, bits));
        }
        self.trim();
        pinned
    }

    /// Merge free buddies, smallest first, into blocks of up to 2^bits
    /// bytes, so that a request for 2^bits can be met.  A grant is not
    /// merged with its buddy: it goes back to memd whole.
    fn merge_up_to(&mut self, bits: usize) {
        loop {
            let mut merged = false;
            let mut i = 0;
            while i < self.blocks.len() {
                let (paddr, b) = (self.blocks[i].paddr, self.blocks[i].size_bits);
                let parent = (paddr & !(1 << b), b + 1);
                let buddy = paddr ^ (1 << b);
                let pslot = self.parents.get(&parent).copied();
                let j = self.blocks.iter().position(|x| x.paddr == buddy && x.size_bits == b);
                let grant = self.grants.contains(&(paddr, b)) || self.grants.contains(&(buddy, b));
                let (Some(pslot), Some(j), true, false) = (pslot, j, b < bits, grant) else {
                    i += 1;
                    continue;
                };
                // Neither half has children: deleting both leaves the
                // parent without children, so its next retype starts
                // afresh (and clears what the halves used).
                let (x, y) = (self.blocks.swap_remove(i.max(j)), self.blocks.swap_remove(i.min(j)));
                for half in [x.cap, y.cap] {
                    let s = half.bits() as Slot;
                    slot::CNODE.cap().absolute_cptr(half).delete().expect("delete merged half");
                    self.slots.global(|sl| sl.free(s));
                }
                self.parents.remove(&parent);
                self.blocks.push(UtRegion::fresh(untyped_at(pslot), parent.0, parent.1));
                merged = true;
            }
            if !merged || self.has_block(bits) {
                return;
            }
        }
    }

    /// Units of memd's table: 2 MiB units up to the end of the last free
    /// block memd will get (`hand_over`).
    pub fn memd_units(&mut self) -> u64 {
        self.merge_up_to(usize::MAX);
        self.blocks.iter().filter(|b| b.size_bits >= BLOCK_BITS).map(|b| ((b.paddr + (1 << b.size_bits)) >> BLOCK_BITS) as u64).max().unwrap_or(0)
    }

    /// Give memd every free block of 2 MiB or more, once it runs: from
    /// then on, memory comes from it.  Smaller blocks (boot Untypeds next
    /// to reserved memory) stay here, for the root task's own objects.
    /// The parents of blocks split here stay here, so memd cannot merge
    /// those blocks with their buddies; only the blocks root split at boot
    /// are concerned.
    pub fn hand_over(&mut self, memd: MemdLink) {
        self.merge_up_to(usize::MAX);
        let (blocks, small) = core::mem::take(&mut self.blocks).into_iter().partition(|b| b.size_bits >= BLOCK_BITS);
        self.blocks = small;
        self.give_back(memd, blocks);
        self.memd = Some(memd);
    }

    /// Return whole free grants to memd, beyond LOCAL_KEEP bytes of free
    /// blocks.  Nothing is merged until there is a grant's worth more than
    /// that: the next retype of a merged block makes seL4 clear all that
    /// its halves used, and merging after every destroy made creating a
    /// guest a fifth slower.
    fn trim(&mut self) {
        let Some(memd) = self.memd else { return };
        if self.local_bytes() < LOCAL_KEEP + (1 << GRANT_BITS) {
            return;
        }
        self.merge_up_to(usize::MAX);
        let mut whole: Vec<usize> = (0..self.blocks.len())
            .filter(|&i| self.grants.contains(&(self.blocks[i].paddr, self.blocks[i].size_bits)))
            .collect();
        // The largest first.
        whole.sort_by_key(|&i| core::cmp::Reverse(self.blocks[i].size_bits));
        let mut keep = self.local_bytes();
        let mut back = Vec::new();
        for i in whole {
            let size = 1usize << self.blocks[i].size_bits;
            if keep >= LOCAL_KEEP + size {
                keep -= size;
                back.push(i);
            }
        }
        back.sort_unstable_by(|a, b| b.cmp(a));
        let blocks: Vec<UtRegion> = back.into_iter().map(|i| self.blocks.swap_remove(i)).collect();
        for b in &blocks {
            self.grants.remove(&(b.paddr, b.size_bits));
        }
        self.give_back(memd, blocks);
    }

    /// Move `blocks` (untouched) to memd and free their slots.
    fn give_back(&mut self, memd: MemdLink, blocks: Vec<UtRegion>) {
        for batch in blocks.chunks(l4h_host::memd::XCHG_SLOTS) {
            let regions: Vec<(cap::Untyped, usize, usize)> = batch.iter().map(|b| (b.cap, b.paddr, b.size_bits)).collect();
            memd.free(&regions);
            for (cap, _, _) in regions {
                self.slots.global(|sl| sl.free(cap.bits() as Slot));
            }
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

/// An arena grows by at least this much (2^bits bytes) when it runs out.
const ARENA_GROW_BITS: usize = 20;

/// Untyped memory dedicated to one guest (or host component).
///
/// An arena starts with the memory its owner is expected to need and
/// grows from the free blocks, a block at a time, if that was too little:
/// host overhead is accounted as it is used, without a safety factor.
pub struct Arena {
    pub pool: UtPool,
    /// Untyped capabilities owned by the guest (slot, paddr, size bits):
    /// the arena's own regions and others (the I/O pool).  Revoking these
    /// destroys every object created from them.
    pub owned: Vec<(Slot, usize, usize)>,
}

impl Arena {
    pub fn own(&mut self, slot: Slot, paddr: usize, bits: usize) {
        self.owned.push((slot, paddr, bits));
    }

    /// Retype `count` objects of `bp` into `dest`, growing the arena if it
    /// has no room.  None if the host is out of memory.
    pub fn retype(&mut self, alloc: &mut Alloc, bp: &ObjectBlueprint, count: usize, dest: Dest) -> Option<usize> {
        if let Some(p) = self.pool.retype(bp, count, dest) {
            return Some(p);
        }
        let need = count << bp.physical_size_bits();
        let bits = (need.next_power_of_two().trailing_zeros() as usize).max(ARENA_GROW_BITS);
        let slot = alloc.slots.alloc();
        let Some(region) = alloc.split(bits, slot) else {
            alloc.slots.free(slot);
            return None;
        };
        self.owned.push((slot, region.paddr, bits));
        self.pool.regions.push(region);
        self.pool.regions.sort_by_key(|r| r.size_bits);
        self.pool.retype(bp, count, dest)
    }

    /// Bytes this arena holds (its own regions, not the I/O pool).
    pub fn bytes(&self) -> usize {
        self.pool.regions.iter().map(|r| r.size()).sum()
    }

    pub fn object(&mut self, alloc: &mut Alloc, bp: ObjectBlueprint) -> Slot {
        let s = alloc.slots.alloc();
        self.retype(alloc, &bp, 1, Dest::root(s))
            .unwrap_or_else(|| panic!("out of host memory allocating {:?}", bp.ty()));
        s
    }

    pub fn object_cap<T: sel4::CapType>(&mut self, alloc: &mut Alloc, bp: ObjectBlueprint) -> sel4::Cap<T> {
        let s = self.object(alloc, bp);
        sel4::init_thread::Slot::<T>::from_index(s).cap()
    }
}

/// Something objects can be allocated from: an arena (which grows from the
/// allocator), or the root task's global pool.
pub struct ObjCtx<'a> {
    pub alloc: &'a mut Alloc,
    pub arena: Option<&'a mut Arena>,
}

impl<'a> ObjCtx<'a> {
    pub fn arena(alloc: &'a mut Alloc, arena: &'a mut Arena) -> Self {
        ObjCtx { alloc, arena: Some(arena) }
    }

    pub fn global(alloc: &'a mut Alloc) -> Self {
        ObjCtx { alloc, arena: None }
    }

    pub fn object(&mut self, bp: ObjectBlueprint) -> Slot {
        match &mut self.arena {
            Some(a) => a.object(self.alloc, bp),
            None => self.alloc.object(bp),
        }
    }


    pub fn cap<T: sel4::CapType>(&mut self, bp: ObjectBlueprint) -> sel4::Cap<T> {
        let s = self.object(bp);
        sel4::init_thread::Slot::<T>::from_index(s).cap()
    }
}
