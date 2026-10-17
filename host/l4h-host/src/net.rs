//! netd (vswitch) <-> client (l4h-vmm) packet queues.
//!
//! A port channel is one shared region with a TX ring (client -> switch)
//! and an RX ring (switch -> client), each of fixed-size frame slots.
//! Frames are copied into and out of the slots.

pub const RING_SIZE: usize = 256;
pub const SLOT_SIZE: usize = 2048;
/// Prefix carried with each frame (virtio-net header, little-endian).
pub const HDR_SIZE: usize = 12;
pub const MAX_FRAME: usize = SLOT_SIZE - HDR_SIZE - 8;

#[repr(C)]
pub struct Slot {
    pub len: u32,
    pub reserved: u32,
    pub hdr: [u8; HDR_SIZE],
    pub data: [u8; SLOT_SIZE - 8 - HDR_SIZE],
}

#[repr(C)]
pub struct RingIdx {
    pub head: u32,
    pub pad0: [u32; 15],
    pub tail: u32,
    pub pad1: [u32; 15],
}

#[repr(C)]
pub struct Port {
    pub tx: RingIdx,
    pub rx: RingIdx,
    pub mac: [u8; 8],
    pub mtu: u32,
    pub link_up: u32,
    pub reserved: [u64; 14],
    pub tx_slots: [Slot; RING_SIZE],
    pub rx_slots: [Slot; RING_SIZE],
}
