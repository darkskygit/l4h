//! The guest kernel l4h ships, which every guest runs (the boot module
//! `guest-kernel`), and its module tree (`guest-kmods`): an EROFS image of
//! its /lib/modules, with its modules and the tree to build more against,
//! which every guest maps read-only (DESIGN.md §5.6).
//!
//! Both are shared: guests map read-only copies of the boot module's own
//! frames (bootmod.rs) rather than copies of its bytes.  Of the kernel,
//! that is the RAM blocks of its image that lie wholly in read-only
//! segments (text and rodata): those blocks have no memory of the guest's
//! behind them.

use sel4::{CapRights, ObjectBlueprint, VmAttributes, cap_type};

use l4h_abi as abi;
use l4h_host::fixed::ArrayVec;

use crate::bootmod::Modules;
use crate::elf::Elf;
use crate::guest::{BLOCK, Guest, abs, root_cap};
use crate::mem::{Alloc, Slot};
use crate::vspace::map_frame;

const PAGE: usize = 4096;

/// Runs of pages whose frames are in consecutive root slots, at most: one
/// for an appended module, a few per carved chunk of a boot loader's
/// (bootmod.rs).  Small: `Host`, which holds them, is on the root task's
/// stack.
const MAX_RUNS: usize = 32;

/// The frames of an image's pages: runs of (offset, root slot, pages).
struct Frames(ArrayVec<(usize, Slot, usize), MAX_RUNS>);

impl Frames {
    /// Add the page at `off`, after every page added so far.  False if
    /// there are too many runs.
    fn push(&mut self, off: usize, slot: Slot) -> bool {
        if let Some((o, s, n)) = self.0.last_mut() {
            if *o + *n * PAGE == off && *s + *n == slot {
                *n += 1;
                return true;
            }
        }
        self.0.push((off, slot, 1)).is_ok()
    }

    /// The root slot of the frame of the page at `off`.
    fn get(&self, off: usize) -> Option<Slot> {
        let &(o, s, _) = self.0.iter().find(|&&(o, _, n)| o <= off && off < o + n * PAGE)?;
        Some(s + (off - o) / PAGE)
    }

    /// Copy the frame of the page at `off`, read-only, into root slot `to`.
    fn copy(&self, off: usize, to: Slot) {
        let from = self.get(off).expect("shared page without a frame");
        abs(root_cap::<cap_type::Null>(to))
            .copy(&abs(root_cap::<cap_type::Granule>(from)), CapRights::read_only())
            .expect("shared frame copy");
    }
}

pub struct Kernel {
    pub elf: Elf<'static>,
    /// The VA of guest-physical address 0: where the image starts.
    pub ram_base: usize,
    /// End of the image in guest RAM, page aligned.
    pub end: usize,
    /// Blocks of the image every guest shares (bit b: GPA block b): all
    /// their pages are boot module pages in read-only segments.
    pub shared: u64,
    /// The frames of the pages of the shared blocks, by GPA.
    frames: Frames,
}

impl Kernel {
    pub fn parse(data: &'static [u8], modules: &Modules) -> Result<Self, &'static str> {
        let elf = Elf::parse(data)?;
        let (lo, hi) = elf.bounds();
        if lo >= hi {
            return Err("no loadable segments");
        }
        let ram_base = lo as usize & !(PAGE - 1);
        let end = (hi as usize - ram_base).next_multiple_of(PAGE);
        let mut k = Self { elf, ram_base, end, shared: 0, frames: Frames(ArrayVec::new()) };
        k.share(modules);
        Ok(k)
    }

    /// The frame of the boot module page holding the whole page at `gpa`,
    /// if that page lies in the file data of a read-only segment.  Pages
    /// a segment covers only in part keep the zeros around it private, as
    /// a guest loads them.
    fn module_frame(&self, gpa: usize, modules: &Modules) -> Option<Slot> {
        let va = self.ram_base + gpa;
        let seg = self.elf.segments().find(|s| {
            let at = s.vaddr as usize;
            !s.writable && at <= va && va + PAGE <= at + s.data.len()
        })?;
        let file = seg.data.as_ptr() as usize + (va - seg.vaddr as usize);
        if file % PAGE != 0 {
            return None;
        }
        modules.frame(file)
    }

    /// Find the blocks guests can share, and their frames.
    fn share(&mut self, modules: &Modules) {
        let blocks = (self.end / BLOCK).min(u64::BITS as usize);
        for b in 0..blocks {
            let (runs, last) = (self.frames.0.len(), self.frames.0.last().copied());
            let all = (0..BLOCK / PAGE).all(|i| {
                let gpa = b * BLOCK + i * PAGE;
                self.module_frame(gpa, modules).is_some_and(|s| self.frames.push(gpa, s))
            });
            if all {
                self.shared |= 1 << b;
            } else {
                // The block stays private: forget its pages.
                while self.frames.0.len() > runs {
                    self.frames.0.pop();
                }
                if let (Some(l), Some(r)) = (last, self.frames.0.last_mut()) {
                    *r = l;
                }
            }
        }
        info!(
            "l4h-root: guest kernel: {} of {} MiB shared by all guests",
            self.shared.count_ones() as usize * BLOCK >> 20,
            self.end.div_ceil(1 << 20)
        );
    }

    /// Copy the frame of shared guest-physical page `gpa` into root slot
    /// `to`, read-only.
    pub fn copy_frame(&self, gpa: usize, to: Slot) {
        self.frames.copy(gpa, to);
    }

    /// The lowest guest-physical address of an initrd.
    pub fn initrd_gpa(&self) -> usize {
        self.end + (1 << 20)
    }
}

/// The module image: its boot module's pages, which each guest maps
/// copies of.
pub struct Kmods {
    frames: Frames,
    size: usize,
}

impl Kmods {
    pub fn new(modules: &Modules, data: &[u8]) -> Option<Self> {
        if data.is_empty() || data.len() as u64 > abi::KMOD_MAX {
            sel4::debug_println!("l4h-root: module image of {} bytes ignored", data.len());
            return None;
        }
        let mut frames = Frames(ArrayVec::new());
        let start = data.as_ptr() as usize;
        for off in (0..data.len()).step_by(PAGE) {
            let ok = start % PAGE == 0 && modules.frame(start + off).is_some_and(|s| frames.push(off, s));
            if !ok {
                sel4::debug_println!("l4h-root: module image at {:#x}: no frames to share, ignored", start);
                return None;
            }
        }
        Some(Self { frames, size: data.len() })
    }

    fn pages(&self) -> usize {
        self.size.div_ceil(PAGE)
    }

    fn radix(&self) -> usize {
        self.pages().next_power_of_two().trailing_zeros() as usize
    }

    /// Host memory a guest's mapping of the image takes: the CNode of its
    /// frames and the page tables (and a directory per GiB) mapping them.
    pub fn bytes(&self) -> usize {
        (32 << self.radix()) + (self.pages().div_ceil(512) + self.pages().div_ceil(512 * 512) + 1) * PAGE
    }

    /// Map the image read-only at KMOD_VA in guest `g`, and record it for
    /// the boot info.  The frame copies are kept in a CNode of the guest's
    /// arena, so destroying the guest deletes them.
    pub fn map(&self, g: &mut Guest, alloc: &mut Alloc) {
        let radix = self.radix();
        let keep = g.arena.object_cap::<cap_type::CNode>(alloc, ObjectBlueprint::CNode { size_bits: radix });
        let tmp = alloc.slots.alloc();
        for i in 0..self.pages() {
            self.frames.copy(i * PAGE, tmp);
            let frame = root_cap::<cap_type::Granule>(tmp);
            let (vspace, va) = (g.vspace, abi::KMOD_VA as usize + i * PAGE);
            map_frame(&mut g.ctx(alloc), vspace, frame, va, CapRights::read_only(), VmAttributes::DEFAULT);
            keep.absolute_cptr_from_bits_with_depth(i as u64, radix).move_(&abs(frame)).expect("keep module image mapping");
        }
        alloc.slots.free(tmp);
        g.kmod_size = self.size;
    }
}
