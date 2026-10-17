//! Runtime for l4h host components spawned by l4h-root.
//!
//! A component provides `fn component_main(info: &'static Info) -> !` and
//! invokes `l4h_rt::component!(component_main)`.  l4h-root starts the
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

#[macro_export]
macro_rules! component {
    ($main:path) => {
        $crate::component!($main, heap_size = 1024 * 1024);
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

            $crate::sel4_panicking_env::register_debug_put_char!($crate::sel4::debug_put_char);

        };

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
}
