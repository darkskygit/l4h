//! Run with tests/run-alloctest.sh.  Exercise root's real allocator and
//! fixed containers on the host; only seL4 capability operations are stubs.
#![allow(dead_code)]

extern crate self as l4h_host;
extern crate self as sel4;

#[path = "../host/l4h-host/src/fixed.rs"]
pub mod fixed;
#[path = "../host/l4h-root/src/mem.rs"]
mod mem;

pub const MAX_GUESTS: usize = 64;
pub const RETYPE_FAN_OUT: usize = 16384;
pub mod guest {
    pub const MAX_GUEST_DEVICES: usize = 16;
}

#[macro_export]
macro_rules! sel4_cfg_usize {
    (ROOT_CNODE_SIZE_BITS) => { 19usize };
}

#[macro_export]
macro_rules! debug_println {
    ($($arg:tt)*) => { println!($($arg)*) };
}

pub mod cap_type {
    #[derive(Clone, Copy)]
    pub struct Null;
    #[derive(Clone, Copy)]
    pub struct CNode;
    #[derive(Clone, Copy)]
    pub struct Untyped;
}

pub trait CapType: Copy {}
impl CapType for cap_type::Null {}
impl CapType for cap_type::CNode {}
impl CapType for cap_type::Untyped {}

#[derive(Clone, Copy)]
pub struct Cap<T>(u64, core::marker::PhantomData<T>);

impl<T> Cap<T> {
    pub fn from_bits(bits: u64) -> Self { Self(bits, core::marker::PhantomData) }
    pub fn bits(&self) -> u64 { self.0 }
    pub fn absolute_cptr<U>(&self, _: U) -> AbsoluteCPtr { AbsoluteCPtr }
    pub fn absolute_cptr_for_self(&self) -> AbsoluteCPtr { AbsoluteCPtr }
    pub fn untyped_retype(&self, _: &ObjectBlueprint, _: &AbsoluteCPtr, _: usize, _: usize) -> Result<(), ()> { Ok(()) }
}

pub mod cap {
    pub type Untyped = crate::Cap<crate::cap_type::Untyped>;
    pub type CNode = crate::Cap<crate::cap_type::CNode>;
}

#[derive(Clone, Copy)]
pub struct AbsoluteCPtr;

impl AbsoluteCPtr {
    pub fn delete(&self) -> Result<(), ()> { Ok(()) }
    pub fn revoke(&self) -> Result<(), ()> { Ok(()) }
    pub fn move_(&self, _: &Self) -> Result<(), ()> { Ok(()) }
}

pub mod init_thread {
    pub struct Slot<T> {
        index: usize,
        marker: core::marker::PhantomData<T>,
    }

    impl<T> Slot<T> {
        pub const fn from_index(index: usize) -> Self { Self { index, marker: core::marker::PhantomData } }
        pub fn cap(&self) -> crate::Cap<T> { crate::Cap::from_bits(self.index as u64) }
    }

    pub mod slot {
        pub const CNODE: super::Slot<crate::cap_type::CNode> = super::Slot::from_index(1);
    }
}

pub enum ObjectBlueprint {
    Untyped { size_bits: usize },
    CNode { size_bits: usize },
    Frame,
}

impl ObjectBlueprint {
    pub fn physical_size_bits(&self) -> usize {
        match self {
            Self::Untyped { size_bits } => *size_bits,
            Self::CNode { size_bits } => size_bits + 5,
            Self::Frame => 12,
        }
    }

    pub fn ty(&self) -> usize { self.physical_size_bits() }
}

pub struct Desc {
    paddr: usize,
    bits: usize,
}

impl Desc {
    pub fn is_device(&self) -> bool { false }
    pub fn paddr(&self) -> usize { self.paddr }
    pub fn size_bits(&self) -> usize { self.bits }
}

pub struct BootInfo(Vec<Desc>);
pub struct UntypedSlots;

impl BootInfo {
    pub fn untyped_list(&self) -> &[Desc] { &self.0 }
    pub fn untyped(&self) -> UntypedSlots { UntypedSlots }
}

impl UntypedSlots {
    pub fn index(&self, i: usize) -> init_thread::Slot<cap_type::Untyped> { init_thread::Slot::from_index(32 + i) }
}

pub mod memd {
    pub const BLOCK_BITS: usize = 21;
    pub const XCHG_SLOTS: usize = 32;

    #[derive(Clone, Copy)]
    pub struct MemdLink;
    pub struct Status {
        pub free_bytes: u64,
        pub reported_bytes: u64,
        pub largest_block: u64,
    }

    impl MemdLink {
        pub fn status(&self) -> Status { panic!("no memd in this test") }
        pub fn alloc(&self, _: usize, _: usize) -> Option<usize> { panic!("no memd in this test") }
        pub fn free(&self, _: &[(crate::cap::Untyped, usize, usize)]) { panic!("no memd in this test") }
    }
}

#[test]
fn arena_growth_on_fragmented_memory_and_reclaim() {
    // Disjoint 2 MiB blocks with occupied holes between them; no buddies
    // can merge.  Slots and allocator tables are taken only once.
    let boot = BootInfo((0..48).map(|i| Desc { paddr: (64 << 20) + i * (4 << 20), bits: 21 }).collect());
    let mut alloc = mem::Alloc::new(mem::Slots::new(1024..mem::ROOT_SLOTS), &boot);
    let mut arena = alloc.arena(32 << 20).unwrap();
    let block = ObjectBlueprint::Untyped { size_bits: 21 };
    for i in 0..16 {
        assert!(arena.retype(&mut alloc, &block, 1, mem::Dest::root(512 + i)).is_some());
    }
    assert_eq!(alloc.free_bytes(), 64 << 20);

    // A 4 MiB request really cannot fit: do not fall back below its size
    // or consume any memory or slots on failure.
    let slots = alloc.slots.available();
    assert!(arena.retype(&mut alloc, &ObjectBlueprint::Untyped { size_bits: 22 }, 1, mem::Dest::root(528)).is_none());
    assert_eq!(alloc.free_bytes(), 64 << 20);
    assert_eq!(alloc.slots.available(), slots);

    // A page does fit, despite the arena's preferred 4 MiB growth size.
    arena.object(&mut alloc, ObjectBlueprint::Frame);
    assert_eq!(arena.bytes(), 34 << 20);
    assert_eq!(alloc.free_bytes(), 62 << 20);
    alloc.reclaim(arena);
    assert_eq!(alloc.free_bytes(), 96 << 20);

    // Exhaustion still fails cleanly and gives back the attempted slot.
    let mut arena = alloc.arena(96 << 20).unwrap();
    for i in 0..48 {
        assert!(arena.retype(&mut alloc, &block, 1, mem::Dest::root(512 + i)).is_some());
    }
    let slots = alloc.slots.available();
    assert!(arena.retype(&mut alloc, &ObjectBlueprint::Frame, 1, mem::Dest::root(560)).is_none());
    assert_eq!(alloc.free_bytes(), 0);
    assert_eq!(alloc.slots.available(), slots);
    alloc.reclaim(arena);
    assert_eq!(alloc.free_bytes(), 96 << 20);
}
