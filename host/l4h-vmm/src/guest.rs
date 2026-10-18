//! Access to the guest: its I/O pool (mapped at IO_POOL_VA) and event pages.

use core::sync::atomic::{AtomicU64, Ordering};

use l4h_abi as abi;
use l4h_rt::host;
use l4h_rt::sel4;

pub use crate::mem::GuestMem;

pub struct Vcpus {
    pub n: usize,
}

impl Vcpus {
    /// Raise event `ev` on vCPU `v`.
    pub fn raise(&self, v: usize, ev: u32) {
        let v = if v < self.n { v } else { 0 };
        let page = host::vmm_event_page_va(v) as *mut abi::EventPage;
        let word = unsafe { &*(core::ptr::addr_of_mut!((*page).pending[(ev / 64) as usize]) as *const AtomicU64) };
        let bit = 1u64 << (ev % 64);
        if word.fetch_or(bit, Ordering::SeqCst) & bit == 0 {
            l4h_rt::cap::<sel4::cap_type::Notification>(host::VCPU_NTFN_SLOT_BASE + v as u64).signal();
        }
    }
}
