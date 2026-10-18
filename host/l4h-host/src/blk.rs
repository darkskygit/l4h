//! blkd <-> client (l4h-vmm) block request queue.
//!
//! A channel is one shared region: a header, a submission ring and a
//! completion ring of fixed-size entries.  Data is never copied here:
//! requests carry guest-physical segments, which blkd validates against
//! the client's I/O pool (configured by l4h-root) and translates for DMA.
//! Guest RAM is never a DMA target.

pub const MAX_SEGS: usize = 16;
pub const RING_SIZE: usize = 128;

pub const OP_READ: u32 = 0;
pub const OP_WRITE: u32 = 1;
pub const OP_FLUSH: u32 = 4;
pub const OP_DISCARD: u32 = 11;
pub const OP_WRITE_ZEROES: u32 = 13;
pub const OP_GET_ID: u32 = 8;

pub const STATUS_OK: u32 = 0;
pub const STATUS_IOERR: u32 = 1;
pub const STATUS_UNSUPP: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Seg {
    pub gpa: u64,
    pub len: u32,
    /// SEG_WRITE: the device writes this segment (reads from the disk).
    pub flags: u32,
}

pub const SEG_WRITE: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Req {
    pub id: u64,
    pub op: u32,
    pub nr_segs: u32,
    pub sector: u64,
    pub segs: [Seg; MAX_SEGS],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Resp {
    pub id: u64,
    pub status: u32,
    pub reserved: u32,
}

/// Single-producer single-consumer ring indices.  `head` is written by the
/// producer, `tail` by the consumer; both increase monotonically.
#[repr(C)]
pub struct RingIdx {
    pub head: u32,
    pub pad0: [u32; 15],
    pub tail: u32,
    pub pad1: [u32; 15],
}

#[repr(C)]
pub struct Channel {
    pub sq: RingIdx,
    pub cq: RingIdx,
    /// Written by blkd before it sets `ready`.
    pub capacity_sectors: u64,
    pub read_only: u32,
    pub ready: u32,
    /// Logical block size in bytes (512 or 4096); requests must be aligned.
    pub block_size: u32,
    pub reserved: [u32; 11],
    pub reqs: [Req; RING_SIZE],
    pub resps: [Resp; RING_SIZE],
}

/// Notification badges of blkd: the client kicked the channel, the device
/// interrupted.
pub const BLKD_BADGE_KICK: u64 = 1 << 0;
pub const BLKD_BADGE_IRQ: u64 = 1 << 1;

/// blkd CSpace slots (beyond the common ones).
pub const BLKD_SLOT_IRQ: u64 = 16;
/// Notification of the client, badged with its "done" bit.
pub const BLKD_SLOT_CLIENT_DONE: u64 = 17;

/// A virtio-pci structure: BAR-relative location, mapped by l4h-root.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PciRegion {
    pub va: u64,
    pub len: u64,
}

pub const BLKD_VIRTIO: u32 = 0;
pub const BLKD_NVME: u32 = 1;

/// Configuration of a blkd driving one disk (virtio-blk or NVMe PCI
/// device) for one client.
#[repr(C)]
pub struct BlkdConfig {
    pub kind: u32,
    /// Refuse writes (in addition to a read-only device).
    pub read_only: u32,
    /// The client sees sectors [lba_start, lba_start + lba_count) of the
    /// disk (512-byte units); lba_count 0 means up to the end.
    pub lba_start: u64,
    pub lba_count: u64,
    /// NVMe: BAR0 (controller registers and doorbells).
    pub bar0: PciRegion,
    pub pci_bdf: u32,
    /// MSI data of MSI-X table entry 0 (the queue interrupt).
    pub msi_data: u32,
    pub common: PciRegion,
    pub notify: PciRegion,
    pub notify_off_multiplier: u32,
    pub reserved0: u32,
    pub isr: PciRegion,
    pub device: PciRegion,
    pub msix_table: PciRegion,
    /// DMA memory: physically contiguous, mapped at dma_va.
    pub dma_va: u64,
    pub dma_paddr: u64,
    pub dma_size: u64,
    /// The client's channel.
    pub chan_va: u64,
    /// The client's data region, its I/O pool: guest-physical
    /// [pool_gpa, pool_gpa + pool_size) is physically contiguous at
    /// pool_paddr.  Every segment must lie in it.
    pub pool_gpa: u64,
    pub pool_paddr: u64,
    pub pool_size: u64,
}

/// The data region of a client: segments must lie in its I/O pool, and are
/// translated with one offset.  Shared with tests/fuzz-virtq.
#[derive(Clone, Copy, Debug)]
pub struct Pool {
    pub gpa: u64,
    pub paddr: u64,
    pub size: u64,
}

impl Pool {
    /// The physical address of guest-physical [gpa, gpa + len), if it lies
    /// in the pool (len > 0).
    pub fn translate(&self, gpa: u64, len: u64) -> Option<u64> {
        let off = gpa.checked_sub(self.gpa)?;
        let end = off.checked_add(len)?;
        (len > 0 && end <= self.size).then(|| self.paddr + off)
    }
}
