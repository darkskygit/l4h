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

/// Frames of physical memory outside RAM, carved on first use, by page.
pub type Frames = BTreeMap<usize, cap::Granule>;

/// Physical memory outside RAM, read and written through the window.
pub struct Phys<'a> {
    pub alloc: &'a mut Alloc,
    pub devmem: &'a mut DevMem,
    pub win: &'a mut Window,
    pub frames: &'a mut Frames,
}

impl<'a> Phys<'a> {
    pub fn new(alloc: &'a mut Alloc, devmem: &'a mut DevMem, win: &'a mut Window, frames: &'a mut Frames) -> Self {
        Phys { alloc, devmem, win, frames }
    }

    /// The frame of the page at `paddr`, carved on first use: None for
    /// RAM, and for device memory already given to something else.
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

    /// One naturally aligned access of `width` bytes (1, 2, 4 or 8) at
    /// `paddr`, mapped uncached: writes `value` if `write`, returns what
    /// it read otherwise.
    pub fn access(&mut self, paddr: usize, width: usize, write: bool, value: u64) -> Option<u64> {
        if !matches!(width, 1 | 2 | 4 | 8) || paddr % width != 0 {
            return None;
        }
        let f = self.frame(paddr)?;
        let off = paddr % PAGE;
        Some(self.win.with_frame_attrs(&abs_of(f), crate::blkd::DEVICE_ATTRS, |p| unsafe {
            let p = p.add(off);
            match (width, write) {
                (1, false) => (p as *const u8).read_volatile() as u64,
                (2, false) => (p as *const u16).read_volatile() as u64,
                (4, false) => (p as *const u32).read_volatile() as u64,
                (_, false) => (p as *const u64).read_volatile(),
                (1, true) => {
                    (p as *mut u8).write_volatile(value as u8);
                    0
                }
                (2, true) => {
                    (p as *mut u16).write_volatile(value as u16);
                    0
                }
                (4, true) => {
                    (p as *mut u32).write_volatile(value as u32);
                    0
                }
                (_, true) => {
                    (p as *mut u64).write_volatile(value);
                    0
                }
            }
        }))
    }
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// The table at `pa`, header included.
pub fn read_table(phys: &mut Phys, pa: usize) -> Option<Vec<u8>> {
    let hdr = phys.read(pa, 36)?;
    let len = u32_at(&hdr, 4) as usize;
    if !(36..=crate::trustd::AML_MAX).contains(&len) {
        return None;
    }
    phys.read(pa, len)
}

/// The addresses of the tables the XSDT (or, for ACPI 1.0, the RSDT) lists.
pub fn tables(bootinfo: &sel4::BootInfoPtr, phys: &mut Phys) -> Vec<usize> {
    // SEL4_BOOTINFO_HEADER_X86_ACPI_RSDP
    const ACPI_RSDP: u64 = 3;
    let Some(rsdp) = crate::bootmod::extra_chunk(bootinfo, ACPI_RSDP) else { return Vec::new() };
    if rsdp.len() < 20 || &rsdp[..8] != b"RSD PTR " {
        return Vec::new();
    }
    let (root, wide) = if rsdp[15] >= 2 && rsdp.len() >= 32 && u64_at(rsdp, 24) != 0 {
        (u64_at(rsdp, 24) as usize, true)
    } else {
        (u32_at(rsdp, 16) as usize, false)
    };
    let Some(table) = read_table(phys, root) else { return Vec::new() };
    let step = if wide { 8 } else { 4 };
    (36..table.len())
        .step_by(step)
        .take_while(|e| e + step <= table.len())
        .map(|e| if wide { u64_at(&table, e) as usize } else { u32_at(&table, e) as usize })
        .collect()
}

/// The first table with signature `sig`, header included.
pub fn find(bootinfo: &sel4::BootInfoPtr, phys: &mut Phys, sig: &[u8; 4]) -> Option<Vec<u8>> {
    for pa in tables(bootinfo, phys) {
        if phys.read(pa, 4)? == sig {
            return read_table(phys, pa);
        }
    }
    None
}

/// The AML trustd runs (trust::TrustdConfig::aml_size): the FADT, the
/// DSDT it names and every SSDT, back to back with their headers.
pub fn aml_tables(bootinfo: &sel4::BootInfoPtr, phys: &mut Phys) -> Option<Vec<u8>> {
    let fadt = find(bootinfo, phys, b"FACP")?;
    let x_dsdt = if fadt.len() >= 148 { u64_at(&fadt, 140) as usize } else { 0 };
    let dsdt = if x_dsdt != 0 { x_dsdt } else { u32_at(&fadt, 40) as usize };
    let mut out = fadt;
    out.extend(read_table(phys, dsdt)?);
    for pa in tables(bootinfo, phys) {
        if phys.read(pa, 4)? == b"SSDT" {
            out.extend(read_table(phys, pa)?);
        }
    }
    (out.len() <= crate::trustd::AML_MAX).then_some(out)
}
