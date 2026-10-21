//! Reading ACPI tables (x86): seL4 parses the ones it needs itself and
//! passes us a copy of the RSDP; the tables lie outside RAM, in device
//! memory.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use sel4::cap;

use crate::component::abs_of;
use crate::devmem::DevMem;
use crate::mem::Alloc;
use crate::window::Window;

const PAGE: usize = 4096;

/// Physical memory outside RAM, read through the window a page at a time.
pub struct Phys<'a> {
    pub alloc: &'a mut Alloc,
    pub devmem: &'a mut DevMem,
    pub win: &'a mut Window,
    /// Frames carved so far, by page.
    frames: BTreeMap<usize, cap::Granule>,
}

impl<'a> Phys<'a> {
    pub fn new(alloc: &'a mut Alloc, devmem: &'a mut DevMem, win: &'a mut Window) -> Self {
        Phys { alloc, devmem, win, frames: BTreeMap::new() }
    }

    /// The frame of the page at `paddr`, carved on first use.
    pub fn frame(&mut self, paddr: usize) -> Option<cap::Granule> {
        let page = paddr & !(PAGE - 1);
        if !self.frames.contains_key(&page) {
            let (alloc, devmem) = (&mut *self.alloc, &mut *self.devmem);
            let f = alloc.slots.global(|slots| devmem.frames(slots, page, PAGE))?;
            self.frames.insert(page, f[0]);
        }
        self.frames.get(&page).copied()
    }

    /// `len` bytes at `paddr`, read with mapping attributes `attrs`.
    pub fn read_attrs(&mut self, paddr: usize, len: usize, attrs: sel4::VmAttributes) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let at = paddr + out.len();
            let n = (PAGE - at % PAGE).min(len - out.len());
            let f = self.frame(at)?;
            self.win.with_frame_attrs(&abs_of(f), attrs, |p| unsafe {
                let src = p.add(at % PAGE);
                for i in 0..n {
                    out.push(src.add(i).read_volatile());
                }
            });
        }
        Some(out)
    }

    pub fn read(&mut self, paddr: usize, len: usize) -> Option<Vec<u8>> {
        self.read_attrs(paddr, len, sel4::VmAttributes::DEFAULT)
    }
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// The ACPI table with signature `sig` (header included), if the firmware
/// has one.
pub fn find(bootinfo: &sel4::BootInfoPtr, phys: &mut Phys, sig: &[u8; 4]) -> Option<Vec<u8>> {
    // SEL4_BOOTINFO_HEADER_X86_ACPI_RSDP
    const ACPI_RSDP: u64 = 3;
    let rsdp = crate::bootmod::extra_chunk(bootinfo, ACPI_RSDP)?;
    if rsdp.len() < 20 || &rsdp[..8] != b"RSD PTR " {
        return None;
    }
    // ACPI 2.0 has the XSDT, with 64-bit entries; 1.0 the RSDT.
    let (root, wide) = if rsdp[15] >= 2 && rsdp.len() >= 32 && u64_at(rsdp, 24) != 0 {
        (u64_at(rsdp, 24) as usize, true)
    } else {
        (u32_at(rsdp, 16) as usize, false)
    };
    let hdr = phys.read(root, 36)?;
    let len = (u32_at(&hdr, 4) as usize).clamp(36, 1 << 16);
    let table = phys.read(root, len)?;
    let step = if wide { 8 } else { 4 };
    for e in (36..len).step_by(step).take_while(|e| e + step <= len) {
        let pa = if wide { u64_at(&table, e) as usize } else { u32_at(&table, e) as usize };
        let h = phys.read(pa, 36)?;
        if &h[..4] == sig {
            let n = (u32_at(&h, 4) as usize).clamp(36, 1 << 16);
            return phys.read(pa, n);
        }
    }
    None
}
