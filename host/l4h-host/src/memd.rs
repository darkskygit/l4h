//! l4h-memd, the owner of the host's free memory and of every guest's RAM
//! (DESIGN.md §16): its CSpace, the calls l4h-root makes to it, and the
//! status page it keeps for l4h-root.
//!
//! memd's CSpace is one CNode of 2^cnode_bits slots (MemdConfig), large
//! enough for every Untyped it can hold.  l4h-root holds a capability to
//! it, and passes Untypeds and CNodes by moving them into the exchange
//! slots and the guest slots below; memd never calls l4h-root.

/// Slot of guest g's block CNode (L4H_SLOT_BLOCKS of the guest), which
/// l4h-root puts there before MEMD_GUEST_CREATE and memd deletes at
/// MEMD_GUEST_DESTROY.
pub const SLOT_GUEST_BCN: u64 = 32;
pub const MAX_GUESTS: usize = 64;
/// Exchange slots: Untypeds passed by MEMD_ALLOC, MEMD_FREE and
/// MEMD_GUEST_COPY, slot SLOT_XCHG + i for the i-th.  A call passes at
/// most XCHG_SLOTS (MEMD_FREE takes two message registers for each).
pub const SLOT_XCHG: u64 = 128;
pub const XCHG_SLOTS: usize = 32;
/// memd allocates its own slots from here.
pub const SLOT_DYN_BASE: u64 = 256;
const _: () = assert!(SLOT_GUEST_BCN as usize + MAX_GUESTS <= SLOT_XCHG as usize);
const _: () = assert!(SLOT_XCHG as usize + XCHG_SLOTS <= SLOT_DYN_BASE as usize);

/// Labels of l4h-root's calls on memd's service endpoint.  The reply's
/// MR0 is 0 or an errno (ENOMEM, EINVAL).
///
/// MR0 = size bits, MR1 = count (<= XCHG_SLOTS): `count` Untypeds of
/// 2^bits bytes without children, in the exchange slots; MR1.. of the
/// reply = their physical addresses.
pub const MEMD_ALLOC: u64 = 1;
/// MR0 = count (<= XCHG_SLOTS), MR1.. = (physical address, size bits) of
/// each: Untypeds without children that l4h-root put in the exchange
/// slots, returned to the free memory.
pub const MEMD_FREE: u64 = 2;
/// MR0 = guest, MR1 = blocks, MR2 = radix of its block CNode: take the
/// guest's RAM, `blocks` 2 MiB blocks.  Nothing is given yet.
pub const MEMD_GUEST_CREATE: u64 = 3;
/// MR0 = guest, MR1 = block: a copy of the block's Untyped in exchange
/// slot 0, for l4h-root to fill with frames (a boot block); reply MR1 =
/// its physical address.  Once per block, before MEMD_GUEST_GIVE.
pub const MEMD_GUEST_COPY: u64 = 4;
/// MR0 = guest, MR1 = first block: copies of the Untypeds of blocks
/// [first, blocks) into the guest's block CNode, at their index.
pub const MEMD_GUEST_GIVE: u64 = 5;
/// MR0 = guest: revoke every block of its RAM, which deletes all that was
/// made of them (its frames and objects, l4h-root's frames of its boot
/// blocks), and free them.  A guest memd does not know is no error.
pub const MEMD_GUEST_DESTROY: u64 = 6;
/// MR0..MR4 = the free page reporting device (BalloonConfig), which
/// l4h-root has mapped: drive it.  Reply MR0 = 0, or ENODEV if it cannot
/// report.
pub const MEMD_BALLOON: u64 = 7;

/// Guest RAM comes in blocks of 2^BLOCK_BITS bytes.
pub const BLOCK_BITS: usize = 21;

/// memd's status page, mapped read-only in l4h-root.  memd updates it
/// after every call and report.
pub const STATUS_VA: usize = crate::SHARED_VA;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Status {
    /// Bytes in free blocks (not given to l4h-root or a guest).
    pub free_bytes: u64,
    /// Of those, reported to the hypervisor.
    pub reported_bytes: u64,
    /// Free blocks, and the size of the largest.
    pub free_blocks: u64,
    pub largest_block: u64,
    /// Bytes of guest RAM memd holds.
    pub guest_bytes: u64,
}

/// The free page reporting device (a virtio-balloon on PCI): its BARs are
/// mapped at BALLOON_VA + bar * BAR_STRIDE, three DMA pages at DMA_VA.
pub const BALLOON_VA: usize = crate::MMIO_VA;
pub const BAR_STRIDE: usize = 0x100_0000;
pub const DMA_VA: usize = crate::DMA_VA;
pub const DMA_PAGES: usize = 3;

/// Where the device's capability structures are: (BAR, offset) of the
/// common, notify and device configuration, packed as bar << 32 | offset.
#[derive(Clone, Copy, Default, Debug)]
pub struct BalloonConfig {
    pub common: u64,
    pub notify: u64,
    pub device: u64,
    pub notify_mult: u64,
    pub dma_paddr: u64,
}

impl BalloonConfig {
    pub fn to_mrs(&self) -> [u64; 5] {
        [self.common, self.notify, self.device, self.notify_mult, self.dma_paddr]
    }

    pub fn from_mrs(m: [u64; 5]) -> Self {
        BalloonConfig { common: m[0], notify: m[1], device: m[2], notify_mult: m[3], dma_paddr: m[4] }
    }

    pub fn va(packed: u64) -> usize {
        BALLOON_VA + (packed >> 32) as usize * BAR_STRIDE + (packed & 0xffff_ffff) as usize
    }
}

/// memd's configuration in its info page.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MemdConfig {
    pub cnode_bits: u64,
}

/// Badge of memd's timer, on its bound notification.  l4h-root calls
/// with an unbadged capability.
pub const BADGE_TIMER: u64 = 1;
