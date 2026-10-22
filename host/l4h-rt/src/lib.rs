//! Runtime for l4h host components spawned by l4h-root.
//!
//! A component provides `fn component_main(info: &'static Info) -> !` and
//! invokes `l4h_rt::component!(component_main, no_heap)` (or `heap_size = N`
//! where an allocator is truly needed, DESIGN.md §9).  l4h-root starts the
//! component at its ELF entry with the stack set up (see l4h-host for the
//! address-space layout).

#![no_std]

pub use l4h_host as host;
pub use sel4;

#[doc(hidden)]
pub use one_shot_mutex;
#[doc(hidden)]
pub use sel4_dlmalloc;
#[doc(hidden)]
pub use sel4_panicking;
#[doc(hidden)]
pub use sel4_panicking_env;
#[doc(hidden)]
pub use sel4_runtime_common;

pub mod ioport;

/// Limits a recurring critical error (such as failed I/O) to its first few
/// prints: each one is a synchronous kernel print (see `host::VERBOSE`).
pub struct ErrorLimit(u32);

impl ErrorLimit {
    const MAX: u32 = 8;

    pub const fn new() -> Self {
        ErrorLimit(0)
    }

    /// Whether to print this occurrence; the last one printed says so.
    pub fn allow(&mut self) -> bool {
        self.0 = self.0.saturating_add(1);
        if self.0 == Self::MAX {
            sel4::debug_println!("(further errors of this kind are not printed)");
        }
        self.0 <= Self::MAX
    }
}

/// An informational message: printed only when `host::VERBOSE` (not in
/// release builds).  Use `sel4::debug_println!` for critical errors only.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        if $crate::host::VERBOSE {
            $crate::sel4::debug_println!($($arg)*);
        }
    };
}

use l4h_host::{CONFIG_OFFSET, INFO_MAGIC, INFO_VA, IPCBUF_VA, InfoHeader};

/// The component info page.
pub struct Info {
    _private: (),
}

impl Info {
    pub fn header(&self) -> &InfoHeader {
        unsafe { &*(INFO_VA as *const InfoHeader) }
    }

    /// Kind-specific configuration.
    ///
    /// # Safety
    /// `T` must be the configuration type l4h-root wrote for this kind.
    pub unsafe fn config<T>(&self) -> &T {
        unsafe { &*((INFO_VA + CONFIG_OFFSET) as *const T) }
    }
}

static INFO: Info = Info { _private: () };

#[doc(hidden)]
pub fn init() -> &'static Info {
    unsafe {
        sel4::set_ipc_buffer((IPCBUF_VA as *mut sel4::IpcBuffer).as_mut().unwrap());
    }
    assert_eq!(INFO.header().magic, INFO_MAGIC, "bad component info page");
    &INFO
}

pub fn cap<T: sel4::CapType>(slot: u64) -> sel4::Cap<T> {
    sel4::Cap::from_bits(slot)
}

/// Where an aborted component ends.  l4h-root calls blkd, netd and memd
/// synchronously and could not wait for their faults to be reported: one
/// that has failed keeps answering on its service endpoint, the call it
/// was serving first, with u64::MAX and zeros (a failed device to root).
/// Any other component faults, which root handles.
#[doc(hidden)]
pub fn abort_trap() -> ! {
    let h = INFO.header();
    if h.magic == INFO_MAGIC && matches!(h.kind, host::KIND_BLKD | host::KIND_NETD | host::KIND_MEMD) {
        let ep = cap::<sel4::cap_type::Endpoint>(host::SLOT_SERVICE_EP);
        let reply = cap::<sel4::cap_type::Reply>(host::SLOT_REPLY);
        loop {
            sel4::with_ipc_buffer_mut(|b| {
                let m = b.msg_regs_mut();
                m[0] = u64::MAX;
                m[1..8].fill(0);
            });
            ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, 8), reply);
        }
    }
    loop {
        unsafe {
            #[cfg(target_arch = "x86_64")]
            core::arch::asm!("ud2");
            #[cfg(target_arch = "aarch64")]
            core::arch::asm!("udf #0");
        }
    }
}

/// The allocator of a component without a heap (`component!(main,
/// no_heap)`): every allocation fails, which aborts.  The component
/// itself cannot allocate, since it does not link `alloc`; this only
/// satisfies the runtime's crates.
#[doc(hidden)]
pub struct NoHeap;

unsafe impl core::alloc::GlobalAlloc for NoHeap {
    unsafe fn alloc(&self, _: core::alloc::Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _: *mut u8, _: core::alloc::Layout) {}
}

#[macro_export]
macro_rules! component {
    (@entry $main:path) => {
        $crate::sel4_panicking_env::register_debug_put_char!($crate::sel4::debug_put_char);
        $crate::sel4_panicking_env::register_abort_trap!($crate::abort_trap);

        $crate::sel4_runtime_common::declare_stack!(256 * 1024);
        $crate::sel4_runtime_common::declare_entrypoint_with_stack_init!();
        $crate::sel4_runtime_common::declare_rust_entrypoint! {
            __l4h_component_entrypoint()
        }

        fn __l4h_component_entrypoint() -> ! {
            let info = $crate::init();
            match $crate::sel4_panicking::catch_unwind(|| $main(info)) {
                #[allow(unreachable_patterns)]
                Ok(never) => never,
                Err(_) => $crate::sel4_panicking_env::abort!("component main() panicked"),
            }
        }
    };
    ($main:path, no_heap) => {
        #[global_allocator]
        static GLOBAL_ALLOCATOR: $crate::NoHeap = $crate::NoHeap;

        $crate::component!(@entry $main);
    };
    ($main:path, heap_size = $heap:expr) => {
        const _: () = {
            use $crate::sel4_dlmalloc::{StaticDlmalloc, StaticHeap};

            const HEAP_SIZE: usize = $heap;
            static STATIC_HEAP: StaticHeap<HEAP_SIZE> = StaticHeap::new();

            #[global_allocator]
            static GLOBAL_ALLOCATOR: StaticDlmalloc<
                $crate::one_shot_mutex::sync::RawOneShotMutex,
            > = StaticDlmalloc::new(STATIC_HEAP.bounds());
        };

        $crate::component!(@entry $main);
    };
}
