//! The guest kernel l4h ships, which every guest runs (the boot module
//! `guest-kernel`), and its module tree (`guest-kmods`): an EROFS image of
//! its /lib/modules, with its modules and the tree to build more against,
//! which every guest maps read-only (DESIGN.md §5.6).

use alloc::vec::Vec;

use sel4::init_thread::slot;
use sel4::{CapRights, cap, cap_type};

use l4h_abi as abi;

use crate::elf::Elf;
use crate::guest::{Guest, abs, root_cap};
use crate::mem::{Alloc, ObjCtx};
use crate::vspace::{LARGE_PAGE_BP, map_large_page};

const PAGE: usize = 4096;
const LARGE_PAGE: usize = 2 << 20;
/// Where the root task maps the module image, to fill it.
const KMOD_ROOT_VA: usize = 0x0000_7600_0000_0000;

pub struct Kernel {
    pub elf: Elf<'static>,
    /// The VA of guest-physical address 0: where the image starts.
    pub ram_base: usize,
    /// End of the image in guest RAM, page aligned.
    pub end: usize,
}

impl Kernel {
    pub fn parse(data: &'static [u8]) -> Result<Self, &'static str> {
        let elf = Elf::parse(data)?;
        let (lo, hi) = elf.bounds();
        if lo >= hi {
            return Err("no loadable segments");
        }
        let ram_base = lo as usize & !(PAGE - 1);
        Ok(Self { elf, ram_base, end: (hi as usize - ram_base).next_multiple_of(PAGE) })
    }

    /// The lowest guest-physical address of an initrd.
    pub fn initrd_gpa(&self) -> usize {
        self.end + (1 << 20)
    }
}

/// The module image, in 2 MiB pages that each guest maps a copy of.
pub struct Kmods {
    pages: Vec<cap::LargePage>,
    size: usize,
}

impl Kmods {
    /// Copy the image out of the boot module, which is neither aligned for
    /// 2 MiB pages nor memory guests can be given.
    pub fn new(alloc: &mut Alloc, data: &[u8]) -> Option<Self> {
        if data.is_empty() || data.len() as u64 > abi::KMOD_MAX {
            sel4::debug_println!("l4h-root: module image of {} bytes ignored", data.len());
            return None;
        }
        let n = data.len().div_ceil(LARGE_PAGE);
        if alloc.free_bytes() < n * LARGE_PAGE * 2 {
            sel4::debug_println!("l4h-root: no memory for the module image");
            return None;
        }
        let pages: Vec<cap::LargePage> = alloc.global(|alloc| {
            (0..n)
                .map(|i| {
                    let p = alloc.object_cap::<cap_type::LargePage>(LARGE_PAGE_BP);
                    let va = KMOD_ROOT_VA + i * LARGE_PAGE;
                    map_large_page(&mut ObjCtx::global(alloc), slot::VSPACE.cap(), p, va, CapRights::read_write());
                    p
                })
                .collect()
        });
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), KMOD_ROOT_VA as *mut u8, data.len()) };
        Some(Self { pages, size: data.len() })
    }

    /// Map the image read-only at KMOD_VA in guest `g`, and record it for
    /// the boot info.  The copies' root slots are tracked into the guest
    /// (`Alloc::slots`), and deleting them unmaps the image.
    pub fn map(&self, g: &mut Guest, alloc: &mut Alloc) {
        let vspace = g.vspace;
        for (i, p) in self.pages.iter().enumerate() {
            let copy = alloc.slots.alloc();
            abs(root_cap::<cap_type::Null>(copy)).copy(&abs(*p), CapRights::read_only()).expect("module image copy");
            let va = abi::KMOD_VA as usize + i * LARGE_PAGE;
            map_large_page(&mut g.ctx(alloc), vspace, root_cap::<cap_type::LargePage>(copy), va, CapRights::read_only());
        }
        g.kmod_size = self.size;
    }
}
