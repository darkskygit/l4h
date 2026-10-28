// SPDX-License-Identifier: GPL-2.0-only
//! Exercise the production C writer and Rust consumer together, including
//! deterministic interleavings while the host writes a serial character.
extern crate self as l4h_rt;
pub use l4h_host as host;

use core::ptr::{addr_of, addr_of_mut};
use l4h_abi as abi;
use std::cell::RefCell;
use std::sync::{Mutex, MutexGuard, Once};
use std::sync::atomic::{AtomicUsize, Ordering};

static LOCK: Mutex<()> = Mutex::new(());
static MAP: Once = Once::new();
static EVENTS: AtomicUsize = AtomicUsize::new(0);
const VCPUS: usize = 2;
const EVENT: u32 = 7;
thread_local! {
    static SERIAL: RefCell<Option<Box<dyn FnMut(u8)>>> = RefCell::new(None);
}

#[macro_export]
macro_rules! debug_print { ($($a:tt)*) => {{ let _ = format_args!($($a)*); }}; }
pub mod sel4 {
    pub use crate::debug_print;
    pub fn debug_put_char(c: u8) {
        crate::SERIAL.with(|s| {
            if let Some(f) = s.borrow_mut().as_mut() { f(c); }
        });
    }
}
mod guest {
    pub struct Vcpus { pub n: usize }
    impl Vcpus {
        pub fn raise(&self, v: usize, event: u32) {
            assert_eq!((v, event), (0, crate::EVENT));
            crate::EVENTS.fetch_add(1, crate::Ordering::Relaxed);
        }
    }
}
#[path = "../host/l4h-vmm/src/console.rs"]
mod console;

unsafe extern "C" {
    fn mmap(a: usize, n: usize, prot: i32, flags: i32, fd: i32, off: usize) -> usize;
    fn console_test_select(page: *mut abi::EventPage);
    fn console_test_reset();
    fn console_test_kicks() -> u32;
    fn l4h_console_try_write(s: *const u8, n: u32) -> u32;
}
fn page(v: usize) -> *mut abi::EventPage { host::vmm_event_page_va(v) as *mut _ }
fn out(v: usize) -> *mut abi::ConsoleOut { unsafe { addr_of_mut!((*page(v)).console) } }
fn head(v: usize) -> u32 { unsafe { addr_of!((*out(v)).head).read_volatile() } }
fn tail(v: usize) -> u32 { unsafe { addr_of!((*out(v)).tail).read_volatile() } }
fn write(v: usize, bytes: &[u8]) -> usize {
    unsafe {
        console_test_select(page(v));
        l4h_console_try_write(bytes.as_ptr(), bytes.len() as u32) as usize
    }
}
fn output() -> Vec<u8> {
    let r = host::VMM_CONSOLE_RING_VA as *const host::ConsoleRing;
    unsafe {
        let n = addr_of!((*r).head).read_volatile() as usize;
        assert!(n <= host::CONSOLE_RING_DATA);
        (&(*r).data)[..n].to_vec()
    }
}
struct Fixture { _lock: MutexGuard<'static, ()>, console: console::Console, vcpus: guest::Vcpus }
impl Fixture {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        MAP.call_once(|| {
            for (a, n) in [(host::vmm_event_page_va(0), VCPUS * 4096), (host::VMM_CONSOLE_RING_VA, 4 * 4096)] {
                assert_eq!(unsafe { mmap(a, n, 3, 0x100022, -1, 0) }, a);
            }
        });
        unsafe {
            core::ptr::write_bytes(page(0) as *mut u8, 0, VCPUS * 4096);
            core::ptr::write_bytes(host::VMM_CONSOLE_RING_VA as *mut u8, 0, 4 * 4096);
            console_test_reset();
        }
        EVENTS.store(0, Ordering::Relaxed);
        Self { _lock: lock, console: console::Console::new(true, None, EVENT), vcpus: guest::Vcpus { n: VCPUS } }
    }
    fn drain(&mut self) -> bool { self.console.drain(&self.vcpus) }
}
impl Drop for Fixture {
    fn drop(&mut self) { SERIAL.with(|s| *s.borrow_mut() = None); }
}

#[test]
fn roundtrip_and_cursor_wrap_preserve_unread_records() {
    let mut f = Fixture::new();
    let full = vec![b'a'; abi::CONSOLE_OUT_SIZE - 8];
    assert_eq!(write(0, &full), full.len());
    assert!(!f.drain());
    let next = vec![b'b'; abi::CONSOLE_OUT_SIZE - 128 - 8];
    assert_eq!(write(0, &next), next.len());
    assert!(!f.drain());
    assert_eq!(head(0), abi::CONSOLE_OUT_WRAP - 128);
    let mut expected = output();
    for i in 0..14 {
        let data = vec![b'A' + i; 256];
        assert_eq!(write(0, &data), data.len());
        expected.extend(data);
    }
    assert!(head(0) < tail(0), "cursor must wrap at a multiple of the ring size");
    assert!(!f.drain());
    assert_eq!(output(), expected);
    assert_eq!(head(0), tail(0));
}

#[test]
fn continuous_producer_cannot_extend_a_drain_without_limit() {
    let mut f = Fixture::new();
    assert_eq!(write(0, &[0]), 1);
    assert_eq!(write(1, &[1]), 1);
    let mut printed = 0;
    SERIAL.with(|s| *s.borrow_mut() = Some(Box::new(move |c| {
        printed += 1;
        // Stop eventually even on the old consumer, to fail instead of hang.
        if printed < 100_000 {
            let v = (1 - c) as usize;
            if head(v) == tail(v) { assert_eq!(write(v, &[v as u8]), 1); }
        }
    })));
    assert!(f.drain());
    assert!(output().len() <= abi::CONSOLE_OUT_SIZE / 9);
    SERIAL.with(|s| *s.borrow_mut() = None);
    f.console.flush(&f.vcpus);
    assert_eq!(head(0), tail(0));
    assert_eq!(head(1), tail(1));
}

#[test]
fn refilled_nonempty_ring_is_carried_to_next_batch_without_a_kick() {
    let mut f = Fixture::new();
    let n = abi::CONSOLE_OUT_SIZE / 2 - 8;
    assert_eq!(write(0, &vec![b'a'; n]), n);
    assert_eq!(write(0, &vec![b'b'; n]), n);
    let mut refilled = false;
    SERIAL.with(|s| *s.borrow_mut() = Some(Box::new(move |c| {
        if c == b'b' && !refilled {
            refilled = true;
            assert_eq!(write(0, &vec![b'c'; n]), n);
        }
    })));
    assert!(f.drain());
    assert_eq!(unsafe { console_test_kicks() }, 1);
    assert!(!f.drain());
    assert_eq!(output(), [vec![b'a'; n], vec![b'b'; n], vec![b'c'; n]].concat());
}

#[test]
fn full_ring_wakes_writer_after_space_is_freed() {
    let mut f = Fixture::new();
    assert_eq!(write(0, &vec![b'x'; abi::CONSOLE_OUT_SIZE - 8]), abi::CONSOLE_OUT_SIZE - 8);
    assert_eq!(write(0, b"retry"), 0);
    assert_eq!(EVENTS.load(Ordering::Relaxed), 0);
    assert!(!f.drain());
    assert_eq!(EVENTS.load(Ordering::Relaxed), 1);
    assert_eq!(write(0, b"retry"), 5);
    assert!(!f.drain());
    assert!(output().ends_with(b"retry"));
}

#[test]
fn budget_does_not_announce_space_in_an_undrained_ring() {
    let mut f = Fixture::new();
    let data = vec![b'x'; abi::CONSOLE_OUT_SIZE - 8];
    assert_eq!(write(0, &data), data.len());
    assert_eq!(write(1, &data), data.len());
    assert_eq!(write(1, b"retry"), 0);
    assert!(f.drain());
    assert_eq!(EVENTS.load(Ordering::Relaxed), 0);
    assert!(!f.drain());
    assert_eq!(EVENTS.load(Ordering::Relaxed), 1);
}

#[test]
fn invalid_guest_cursor_or_record_is_skipped() {
    let mut f = Fixture::new();
    unsafe { addr_of_mut!((*out(0)).head).write_volatile(u32::MAX); }
    assert!(!f.drain());
    assert!(tail(0) < abi::CONSOLE_OUT_WRAP);
    unsafe { addr_of_mut!((*out(0)).head).write_volatile(tail(0)); }
    assert_eq!(write(0, b"bad"), 3);
    unsafe {
        for i in 0..4 { (*out(0)).data[(tail(0) as usize + 4 + i) % abi::CONSOLE_OUT_SIZE] = 0xff; }
    }
    assert!(!f.drain());
    assert_eq!(head(0), tail(0));
    assert_eq!(write(0, b"good"), 4);
    assert!(!f.drain());
    assert_eq!(output(), b"good");
}
