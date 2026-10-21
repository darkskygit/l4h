//! l4h-trustd: the attestation service (DESIGN.md §9.2).  Each guest has
//! an attestation device (L4H_DEV_ATTEST): a request page, which trustd
//! maps at page_va(guest), and the kick notification, badged with
//! kick_badge(guest).  trustd answers in the page and has root raise the
//! device's event (ROOT_CALL_ATTEST_DONE).  l4h-root tells trustd about
//! guests through the registry page, which it writes and trustd only
//! reads: root never waits for trustd, whose TPM commands can take
//! seconds.

/// The TPM interface.
pub const TPM_TIS: u32 = 1;
pub const TPM_CRB: u32 = 2;

/// How a CRB command starts (the ACPI TPM2 start method).
/// The start register (7).
pub const START_CRB: u32 = 1;
/// The start register, then the TPM device's _DSM (2 and 8: AMD fTPM).
/// trustd runs the firmware's AML for it (aml.rs).
pub const START_ACPI: u32 = 2;
/// The Pluton doorbells after each request (13).
pub const START_PLUTON: u32 = 3;

/// A physical range trustd has mapped, at TPM_VA + i * RANGE_STRIDE for
/// range i.  size 0: unused.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Range {
    pub paddr: u64,
    pub size: u64,
}

pub const MAX_RANGES: usize = 6;
pub const RANGE_STRIDE: usize = 1 << 20;

/// trustd's configuration (info page).  The registers and buffers are
/// given by their VAs in trustd, in the mapped ranges; 0 if absent.
#[repr(C)]
pub struct TrustdConfig {
    pub iface: u32,
    pub start: u32,
    pub ranges: [Range; MAX_RANGES],
    /// FIFO: locality 0's registers.  CRB: its locality registers, which
    /// the ACPI start and Pluton interfaces lack.
    pub loc_va: u64,
    /// CRB: the control area (CTRL_REQ, ...).
    pub ctrl_va: u64,
    pub cmd_va: u64,
    pub cmd_size: u64,
    pub rsp_va: u64,
    pub rsp_size: u64,
    /// Pluton: the start and reply doorbells.
    pub start_va: u64,
    pub reply_va: u64,
    /// START_ACPI: the firmware's ACPI tables at AML_VA, back to back with
    /// their headers: the FADT, the DSDT, then the SSDTs.
    pub aml_size: u64,
}

pub const TPM_VA: usize = crate::MMIO_VA;
/// The registry page, read-only in trustd.
pub const REGISTRY_VA: usize = crate::SHARED_VA;
pub const AML_VA: usize = crate::SHARED_VA + 0x100_0000;
/// The request pages, one per guest id; trustd owns them, so that they
/// stay mapped while it answers a guest that is being destroyed.
pub const PAGES_VA: usize = crate::SHARED_VA + 0x1000;
pub const fn page_va(guest: usize) -> usize {
    PAGES_VA + guest * 4096
}
/// The kick notification: guest g signals it with kick_badge(g).  The
/// bound notification (SLOT_NTFN) is trustd's timer.
pub const SLOT_KICK: u64 = crate::SLOT_FIRST_FREE;
pub const fn kick_badge(guest: usize) -> u64 {
    1 << guest
}
/// Most ACPI table bytes root gives trustd.
pub const AML_MAX: usize = 4 << 20;

/// The record of guest `id` at REGISTRY_VA + id * size_of::<GuestRecord>().
/// root fills it before the guest starts and writes `instance` last
/// (non-zero: a running guest); it zeroes `instance` first when the guest
/// is destroyed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GuestRecord {
    /// Unique per guest started since boot, from 1.
    pub instance: u64,
    pub ram: u64,
    pub vcpus: u32,
    pub flags: u32,
    /// The launch digest (DESIGN.md §9.2): SHA-256 of the kernel image,
    /// initramfs and command line the guest booted with.
    pub launch: [u8; 32],
    pub reserved: u64,
}

pub const RECORD_DOM0: u32 = 1;
pub const MAX_RECORDS: usize = 64;
const _: () = assert!(core::mem::size_of::<GuestRecord>() == 64);
const _: () = assert!(MAX_RECORDS * core::mem::size_of::<GuestRecord>() <= 4096);
const _: () = assert!(MAX_RECORDS <= 64 && page_va(MAX_RECORDS) <= AML_VA);

/// trustd -> root (ROOT_CALL_HW): an access the firmware's AML makes
/// outside trustd's own ranges.  MR0 = HW_*, MR1 = physical address, I/O
/// port or PCI function (bus << 8 | device << 3 | function), MR2 = PCI
/// config offset, MR3 = width in bytes, MR4 = the value to write.  Returns
/// the value read; root refuses RAM, memory and ports in use, and the
/// config space of the PCI functions the host drives (reads give all ones,
/// writes are dropped).
pub const HW_MEM_READ: u64 = 0;
pub const HW_MEM_WRITE: u64 = 1;
pub const HW_IO_READ: u64 = 2;
pub const HW_IO_WRITE: u64 = 3;
pub const HW_PCI_READ: u64 = 4;
pub const HW_PCI_WRITE: u64 = 5;
