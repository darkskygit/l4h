//! l4h-trustd: the attestation service (DESIGN.md §9.2).  Guests call it
//! on their L4H_SLOT_ATTEST_EP with the badge below; l4h-root tells it
//! about guests through the registry page, which it writes and trustd only
//! reads, so that root never waits for trustd (whose TPM commands can take
//! seconds).

/// The TPM interface (ACPI TPM2 start method).
pub const TPM_TIS: u32 = 1;
pub const TPM_CRB: u32 = 2;

/// trustd's configuration (info page).  The TPM's registers, and for CRB
/// its command and response buffers, are mapped at TPM_VA + their offset
/// from `mmio_paddr`.
#[repr(C)]
pub struct TrustdConfig {
    pub iface: u32,
    pub reserved: u32,
    pub mmio_paddr: u64,
    pub mmio_size: u64,
    /// CRB: offsets (from mmio_paddr) and sizes of the buffers.
    pub cmd_off: u64,
    pub cmd_size: u64,
    pub rsp_off: u64,
    pub rsp_size: u64,
}

pub const TPM_VA: usize = crate::MMIO_VA;
/// The registry page, read-only in trustd.
pub const REGISTRY_VA: usize = crate::SHARED_VA;

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

/// Badge of a guest's capability to trustd's endpoint: this bit and the
/// guest id.
pub const BADGE_GUEST: u64 = 1 << 32;
