//! Access to the guest: its RAM (mapped at GUEST_RAM_VA) and event pages.

use core::sync::atomic::{AtomicU64, Ordering};

use l4h_abi as abi;
use l4h_rt::host;
use l4h_rt::sel4;

pub struct GuestMem {
    pub size: u64,
}

impl GuestMem {
    pub fn valid(&self, gpa: u64, len: u64) -> bool {
        gpa.checked_add(len).is_some_and(|end| end <= self.size)
    }

    fn ptr<T>(&self, gpa: u64) -> *mut T {
        assert!(self.valid(gpa, core::mem::size_of::<T>() as u64), "guest access out of range: {:#x}", gpa);
        (host::GUEST_RAM_VA + gpa as usize) as *mut T
    }

    /// Read a value the guest may change concurrently.  Callers validate
    /// addresses that come from the guest; this panics on bad ones only as
    /// a last line of defence.
    pub fn read<T: Copy>(&self, gpa: u64) -> T {
        unsafe { self.ptr::<T>(gpa).read_volatile() }
    }

    pub fn write<T: Copy>(&self, gpa: u64, v: T) {
        unsafe { self.ptr::<T>(gpa).write_volatile(v) }
    }
}

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
