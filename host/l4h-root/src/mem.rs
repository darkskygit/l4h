//! Untyped memory and CSpace slot management for the root task.
//!
//! All kernel objects are carved out of Untyped capabilities.  We keep our
//! own copy of each Untyped's watermark so that we know the physical address
//! of every child we create (seL4 aligns each retyped object to its size,
//! starting at the current watermark).  Guest RAM relies on this to obtain
//! physically contiguous chunks.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::ops::Range;

use sel4::init_thread::slot;
use sel4::{ObjectBlueprint, cap};

pub type Slot = usize;

/// Allocator for slots in the root task's own CNode.
///
/// While tracking is on, every slot allocated (and not freed again) is
/// recorded, so that the slots holding a guest's capabilities can be
/// cleaned up and reused when the guest is destroyed.
pub struct Slots {
    free: Range<usize>,
    recycled: Vec<usize>,
    track: Option<BTreeSet<Slot>>,
}

impl Slots {
    pub fn new(range: Range<usize>) -> Self {
        Self { free: range, recycled: Vec::new(), track: None }
    }

    pub fn alloc(&mut self) -> Slot {
        let s = match self.recycled.pop() {
            Some(s) => s,
            None => self.free.next().expect("root CNode full"),
        };
        if let Some(t) = &mut self.track {
            t.insert(s);
        }
        s
    }

    /// Allocate `n` consecutive slots.
    pub fn alloc_range(&mut self, n: usize) -> Range<Slot> {
        let start = self.free.start;
        assert!(start + n <= self.free.end, "root CNode full");
        self.free.start += n;
        if let Some(t) = &mut self.track {
            t.extend(start..start + n);
        }
        start..start + n
    }

    pub fn free(&mut self, s: Slot) {
        if let Some(t) = &mut self.track {
            t.remove(&s);
        }
        self.recycled.push(s);
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

    fn largest_fit_bits(&self) -> Option<usize> {
        (12..=self.size_bits).rev().find(|&b| self.fit(b, 1).is_some())
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
        let idx = self.regions.iter().position(|r| r.fit(bits, count).is_some())?;
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
pub struct Alloc {
    pub slots: Slots,
    pub pool: UtPool,
    /// Slots holding Untyped capabilities of `pool` that were reclaimed
    /// from guests: they must survive the cleanup of the guest's slots.
    pub pinned: BTreeSet<Slot>,
}

impl Alloc {
    /// Allocate a single object into a fresh root slot.
    pub fn object(&mut self, bp: ObjectBlueprint) -> Slot {
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

    /// Destroy every object of `arena` and return its memory to the pool.
    pub fn reclaim(&mut self, arena: Arena) {
        for (slot, paddr, bits) in arena.owned {
            let cap = sel4::init_thread::Slot::<sel4::cap_type::Untyped>::from_index(slot).cap();
            slot::CNODE.cap().absolute_cptr(cap).revoke().expect("revoke arena untyped");
            self.pool.regions.push(UtRegion::fresh(cap, paddr, bits));
            self.pinned.insert(slot);
        }
        self.pool.regions.sort_by_key(|r| r.size_bits);
    }

    /// Carve a per-guest arena of at least `bytes` out of the global pool.
    pub fn arena(&mut self, bytes: usize) -> Option<Arena> {
        let mut arena = Arena { pool: UtPool::empty(), owned: Vec::new() };
        let mut remaining = bytes;
        while remaining > 0 {
            let max_bits = self.pool.regions.iter().filter_map(UtRegion::largest_fit_bits).max()?;
            let want_bits = remaining.max(4096).next_power_of_two().trailing_zeros() as usize;
            let bits = want_bits.min(max_bits);
            let slot = self.slots.alloc();
            let region = self.pool.split(bits, slot)?;
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
