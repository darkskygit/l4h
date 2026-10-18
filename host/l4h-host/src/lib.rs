//! Definitions shared by l4h-root and the host components it spawns
//! (l4h-vmm, blkd, netd): the component address-space and CSpace layout,
//! the component info page, and the inter-component queue formats.

#![no_std]

pub mod blk;
pub mod net;

/// Component address-space layout.
pub const INFO_VA: usize = 0x1000_0000;
pub const INFO_SIZE: usize = 0x4000;
pub const IPCBUF_VA: usize = 0x1001_0000;
pub const STACK_TOP: usize = 0x1100_0000;
pub const STACK_SIZE: usize = 512 * 1024;
/// Shared regions (device pages, queues, buffers) are mapped from here.
pub const SHARED_VA: usize = 0x2000_0000_0000;
/// Device MMIO (drivers).
pub const MMIO_VA: usize = 0x3000_0000_0000;
/// DMA memory (drivers).
pub const DMA_VA: usize = 0x3800_0000_0000;
/// Guest RAM (l4h-vmm): GPA g is mapped at GUEST_RAM_VA + g.
pub const GUEST_RAM_VA: usize = 0x4000_0000_0000;

/// Component CSpace: one CNode of 2^CNODE_BITS slots resolving the whole
/// CPtr, so a CPtr is simply a slot number.
pub const CNODE_BITS: usize = 12;
pub const SLOT_CNODE: u64 = 1;
pub const SLOT_VSPACE: u64 = 2;
pub const SLOT_TCB: u64 = 3;
/// Endpoint to l4h-root (badged with the component id).
pub const SLOT_ROOT_EP: u64 = 4;
/// Notification bound to the component's TCB.
pub const SLOT_NTFN: u64 = 5;
/// Endpoint the component serves (if any).
pub const SLOT_SERVICE_EP: u64 = 6;
/// Reply object for receiving on SLOT_SERVICE_EP.
pub const SLOT_REPLY: u64 = 7;
/// First component-specific slot.
pub const SLOT_FIRST_FREE: u64 = 16;

pub const INFO_MAGIC: u64 = 0x4f464e4948344c00; // "\0L4HINFO"

pub const KIND_VMM: u32 = 1;
pub const KIND_BLKD: u32 = 2;
pub const KIND_NETD: u32 = 3;

/// Driver address-space layout: the device structures, then DMA memory.
pub const fn blkd_chan_va() -> usize {
    SHARED_VA
}

/// Header of the component info page at INFO_VA.  The kind-specific
/// configuration follows at `config`.
#[repr(C)]
pub struct InfoHeader {
    pub magic: u64,
    pub kind: u32,
    pub id: u32,
    pub tsc_hz: u64,
    pub reserved: [u64; 5],
}

pub const CONFIG_OFFSET: usize = 64;

/// Labels of calls from components to l4h-root (SLOT_ROOT_EP).
pub const ROOT_CALL_GUEST_POWER: u64 = 1; // MR0 = guest id, MR1 = abi POWER_*
pub const ROOT_CALL_LOG: u64 = 2;
/// l4h-vmm has initialised the device pages: start the guest (MR0 = guest id).
pub const ROOT_CALL_VMM_READY: u64 = 3;
/// dom0's l4h-vmm: execute the control request in dom0's control page.
pub const ROOT_CALL_CTL: u64 = 4;

/// Badge bit set on every endpoint capability l4h-root gives to a guest
/// for l4h-vmm's service endpoint; notification badges never have it.
pub const VMM_BADGE_HYPERCALL: u64 = 1 << 63;

/// Per-guest l4h-vmm configuration.
pub const VMM_MAX_DEVICES: usize = 16;
pub const VMM_MAX_RAM_CHUNKS: usize = 16;

/// Device kinds served by l4h-vmm.
pub const VDEV_CONSOLE: u32 = 0;
pub const VDEV_BLK: u32 = 1;
pub const VDEV_NET: u32 = 2;
pub const VDEV_CTL: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VmmDevice {
    pub kind: u32,
    /// Index in the guest's boot info device table.
    pub guest_index: u32,
    /// Event line raised on the guest vCPU.
    pub event: u32,
    pub reserved0: u32,
    /// VA of the shared device page in the VMM.
    pub page_va: u64,
    /// Backend channel (VDEV_BLK / VDEV_NET): queue region VA and size,
    /// notification slot to kick the backend server.
    pub chan_va: u64,
    pub chan_size: u64,
    pub chan_kick_slot: u64,
    pub mac: [u8; 8],
    /// Block devices: capacity in 512-byte sectors, read-only flag.
    pub capacity: u64,
    pub read_only: u32,
    pub reserved1: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RamChunk {
    pub gpa: u64,
    pub paddr: u64,
    pub size: u64,
}

#[repr(C)]
pub struct VmmConfig {
    pub guest_id: u32,
    pub nr_vcpus: u32,
    pub ram_size: u64,
    pub nr_devices: u32,
    pub nr_ram_chunks: u32,
    /// Slot of the badged (L4H_BADGE_EVENT) notification of vCPU v is
    /// VCPU_NTFN_SLOT_BASE + v; its event page is at event_page_va(v).
    pub dom0: u32,
    /// Also copy console output to the host serial port (with a prefix
    /// unless dom0).
    pub serial: u32,
    pub ram_chunks: [RamChunk; VMM_MAX_RAM_CHUNKS],
    pub devices: [VmmDevice; VMM_MAX_DEVICES],
}

pub const VCPU_NTFN_SLOT_BASE: u64 = 64;
pub const fn vmm_event_page_va(v: usize) -> usize {
    SHARED_VA + v * 0x1000
}
pub const fn vmm_device_page_va(d: usize) -> usize {
    SHARED_VA + 0x100_0000 + d * 0x1000
}
/// Console output ring of the guest, shared by l4h-vmm (writer) and
/// l4h-root (reader).
pub const VMM_CONSOLE_RING_VA: usize = SHARED_VA + 0x80_0000;
pub const CONSOLE_RING_PAGES: usize = 4;
pub const CONSOLE_RING_DATA: usize = CONSOLE_RING_PAGES * 4096 - 64;

#[repr(C)]
pub struct ConsoleRing {
    /// Total bytes ever written; byte i is at data[i % CONSOLE_RING_DATA].
    pub head: u64,
    pub reserved: [u64; 7],
    pub data: [u8; CONSOLE_RING_DATA],
}

pub const fn vmm_chan_va(d: usize) -> usize {
    SHARED_VA + 0x1000_0000 + d * 0x100_0000
}

/// Badges of the VMM's bound notification: bit d = kick from guest device d,
/// bit 32 + d = completion from the backend of device d.
pub const VMM_BADGE_KICK_SHIFT: u32 = 0;
pub const VMM_BADGE_DONE_SHIFT: u32 = 32;

/// The architectural time counter that seL4 deadlines (seL4_TCB_SetTimer)
/// are expressed in: the TSC on x86_64, the virtual counter on aarch64.
pub mod ticks {
    #[cfg(target_arch = "x86_64")]
    pub fn now() -> u64 {
        let lo: u32;
        let hi: u32;
        unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
        ((hi as u64) << 32) | lo as u64
    }

    #[cfg(target_arch = "aarch64")]
    pub fn now() -> u64 {
        let v: u64;
        unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)) };
        v
    }

    /// Counter frequency, where the architecture defines a register for it.
    #[cfg(target_arch = "aarch64")]
    pub fn hz() -> u64 {
        let v: u64;
        unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)) };
        v
    }
}
