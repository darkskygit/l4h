//! Device DMA and interrupt confinement, selected from the firmware tables.

use crate::{amdvi, mem::Alloc, pci};
#[cfg(target_arch = "x86_64")]
use crate::vtd;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    None,
    Amd(amdvi::Domain),
    #[cfg(target_arch = "x86_64")]
    Intel(vtd::Domain),
}

impl Domain {
    pub const NONE: Self = Self::None;
}

pub enum Iommu {
    Amd(amdvi::AmdVi),
    #[cfg(target_arch = "x86_64")]
    Intel(vtd::Vtd),
}

impl Iommu {
    #[cfg(not(target_arch = "x86_64"))]
    pub fn none() -> Self { Self::Amd(amdvi::AmdVi::none()) }
    pub fn off() -> Self { Self::Amd(amdvi::AmdVi::off()) }

    #[cfg(target_arch = "x86_64")]
    pub fn init(bootinfo: &sel4::BootInfoPtr, phys: &mut crate::acpi::Phys, pci: &pci::Pci,
        ticks_per_ms: u64, intremap: bool) -> Self {
        if let Some(vtd) = vtd::Vtd::init(bootinfo, phys, pci, ticks_per_ms, intremap) {
            Self::Intel(vtd)
        } else {
            Self::Amd(amdvi::AmdVi::init(bootinfo, phys, pci, ticks_per_ms, intremap))
        }
    }

    pub fn info(&self) -> [u64; 6] {
        match self {
            Self::Amd(a) => a.info(),
            #[cfg(target_arch = "x86_64")]
            Self::Intel(i) => i.info(),
        }
    }

    /// VT-d blocks compatibility-format interrupts globally. seL4 owns
    /// the I/O APIC and cannot program remappable entries, so root polls
    /// the UART instead of enabling its pin in that mode.
    pub fn poll_serial(&self) -> bool {
        match self {
            #[cfg(target_arch = "x86_64")]
            Self::Intel(i) => i.info()[1] != 0,
            _ => false,
        }
    }

    pub fn attach(&mut self, alloc: &mut Alloc, device: &pci::Device) -> Domain {
        match self {
            Self::Amd(a) => match a.attach(alloc, device) {
                amdvi::Domain::NONE => Domain::None,
                d => Domain::Amd(d),
            },
            #[cfg(target_arch = "x86_64")]
            Self::Intel(i) => i.attach(alloc, device).map_or(Domain::None, Domain::Intel),
        }
    }

    pub fn map(&mut self, alloc: &mut Alloc, domain: Domain, pa: usize, len: usize, write: bool) {
        match (self, domain) {
            (_, Domain::None) => {},
            (Self::Amd(a), Domain::Amd(d)) => a.map(alloc, d, pa, len, write),
            #[cfg(target_arch = "x86_64")]
            (Self::Intel(i), Domain::Intel(d)) => i.map(alloc, d, pa, len, write),
            #[cfg(target_arch = "x86_64")]
            _ => panic!("IOMMU domain belongs to another backend"),
        }
    }

    pub fn unmap(&mut self, domain: Domain, pa: usize, len: usize) {
        match (self, domain) {
            (_, Domain::None) => {},
            (Self::Amd(a), Domain::Amd(d)) => a.unmap(d, pa, len),
            #[cfg(target_arch = "x86_64")]
            (Self::Intel(i), Domain::Intel(d)) => i.unmap(d, pa, len),
            #[cfg(target_arch = "x86_64")]
            _ => panic!("IOMMU domain belongs to another backend"),
        }
    }

    /// The address AND data to put in an MSI/MSI-X entry. Intel carries
    /// the remapping index in the address; AMD carries it in the data.
    pub fn msi(&mut self, domain: Domain, vector: u64) -> (u64, u32) {
        match (self, domain) {
            (_, Domain::None) => (pci::msi_address(), pci::msi_data(vector)),
            (Self::Amd(a), Domain::Amd(d)) => (pci::msi_address(), a.msi_data(d, vector)),
            #[cfg(target_arch = "x86_64")]
            (Self::Intel(i), Domain::Intel(d)) => i.msi(d, vector),
            #[cfg(target_arch = "x86_64")]
            _ => panic!("IOMMU domain belongs to another backend"),
        }
    }

    pub fn poll(&mut self) {
        match self {
            Self::Amd(a) => a.poll(),
            #[cfg(target_arch = "x86_64")]
            Self::Intel(i) => i.poll(),
        }
    }
}
