//! virtio-l4h device model: the shared register page and its commit ops
//! (L4H_HC_DEV_WRITE), independent of the device type.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use l4h_abi as abi;

use crate::guest::{GuestMem, Vcpus};
use crate::virtq::Queue;

pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
pub const VIRTIO_RING_F_INDIRECT_DESC: u64 = 1 << 28;

const STATUS_FEATURES_OK: u32 = 8;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FAILED: u32 = 128;

/// What a device type implements on top of the common model.
pub trait Backend {
    fn device_id(&self) -> u32;
    fn features(&self) -> u64;
    fn num_queues(&self) -> usize;
    fn queue_max(&self, q: usize) -> u32;
    /// Write the device configuration space.
    fn init_config(&self, config: &mut [u8]);
    /// Process available buffers on queue `q`.  Returns true if buffers
    /// were returned to the driver.
    fn kick(&mut self, mem: &GuestMem, queues: &mut [Queue], q: usize) -> bool;
    /// Backend completions arrived.  Returns true if buffers were returned.
    fn complete(&mut self, mem: &GuestMem, queues: &mut [Queue]) -> bool;
    /// The driver reset the device.
    fn reset(&mut self);
    /// The driver accepted `features` (FEATURES_OK).
    fn features_accepted(&mut self, _features: u64) {}
}

pub struct Device<B: Backend> {
    regs: *mut abi::VirtioRegs,
    pub event: u32,
    queues: [Queue; abi::VIRTIO_MAX_QUEUES],
    features: u64,
    pub backend: B,
}

impl<B: Backend> Device<B> {
    pub fn new(page_va: usize, event: u32, backend: B) -> Self {
        let d = Device {
            regs: page_va as *mut abi::VirtioRegs,
            event,
            queues: Default::default(),
            features: backend.features() | VIRTIO_F_VERSION_1,
            backend,
        };
        unsafe {
            let config = core::slice::from_raw_parts_mut(
                (page_va + abi::VIRTIO_CONFIG_OFF) as *mut u8,
                abi::VIRTIO_CONFIG_SIZE,
            );
            config.fill(0);
            d.backend.init_config(config);
            let r = d.regs;
            addr_of_mut!((*r).version).write_volatile(1);
            addr_of_mut!((*r).device_id).write_volatile(d.backend.device_id());
            addr_of_mut!((*r).vendor_id).write_volatile(0x6c34);
            addr_of_mut!((*r).device_features).write_volatile(d.features);
            addr_of_mut!((*r).num_queues).write_volatile(d.backend.num_queues() as u32);
            for q in 0..d.backend.num_queues() {
                addr_of_mut!((*r).queues[q].num_max).write_volatile(d.backend.queue_max(q));
            }
            core::sync::atomic::fence(Ordering::Release);
            addr_of_mut!((*r).magic).write_volatile(abi::VIRTIO_MAGIC);
        }
        d
    }

    fn status(&self) -> u32 {
        unsafe { addr_of!((*self.regs).status).read_volatile() }
    }

    fn set_status(&self, s: u32) {
        unsafe { addr_of_mut!((*self.regs).status).write_volatile(s) }
    }

    fn reset(&mut self) {
        for q in self.queues.iter_mut() {
            q.reset();
        }
        self.backend.reset();
        unsafe {
            let r = self.regs;
            addr_of_mut!((*r).driver_features).write_volatile(0);
            for q in 0..abi::VIRTIO_MAX_QUEUES {
                addr_of_mut!((*r).queues[q].ready).write_volatile(0);
            }
            self.isr().store(0, Ordering::SeqCst);
            self.kick_word().store(0, Ordering::SeqCst);
        }
        self.set_status(0);
    }

    fn isr(&self) -> &AtomicU32 {
        unsafe { &*(addr_of!((*self.regs).isr) as *const AtomicU32) }
    }

    fn kick_word(&self) -> &AtomicU64 {
        unsafe { &*(addr_of!((*self.regs).kick) as *const AtomicU64) }
    }

    fn irq_vcpu(&self) -> usize {
        unsafe { addr_of!((*self.regs).irq_vcpu).read_volatile() as usize }
    }

    /// L4H_HC_DEV_WRITE.
    pub fn op(&mut self, mem: &GuestMem, vcpus: &Vcpus, op: u64, a: u64, _b: u64) -> u64 {
        match op {
            abi::VIRTIO_OP_STATUS => {
                let s = self.status();
                if s == 0 {
                    self.reset();
                } else if s & STATUS_FEATURES_OK != 0 {
                    let drv = unsafe { addr_of!((*self.regs).driver_features).read_volatile() };
                    if drv & !self.features != 0 || drv & VIRTIO_F_VERSION_1 == 0 {
                        self.set_status(s & !STATUS_FEATURES_OK);
                    } else {
                        self.backend.features_accepted(drv);
                    }
                }
                if s & STATUS_DRIVER_OK != 0 && s & STATUS_FAILED == 0 {
                    // Buffers may have been queued before DRIVER_OK.
                    self.poll(mem, vcpus, u64::MAX);
                }
                0
            }
            abi::VIRTIO_OP_FEATURES => 0,
            abi::VIRTIO_OP_QUEUE => {
                let q = a as usize;
                if q >= self.backend.num_queues() {
                    return u64::MAX;
                }
                let regs = unsafe { addr_of!((*self.regs).queues[q]).read_volatile() };
                if regs.ready == 0 {
                    self.queues[q].reset();
                    return 0;
                }
                if !self.queues[q].setup(mem, regs.num, self.backend.queue_max(q), regs.desc, regs.driver, regs.device) {
                    unsafe { addr_of_mut!((*self.regs).queues[q].ready).write_volatile(0) };
                    return u64::MAX;
                }
                0
            }
            abi::VIRTIO_OP_CONFIG => 0,
            abi::VIRTIO_OP_IRQ_VCPU => {
                if self.irq_vcpu() >= vcpus.n {
                    unsafe { addr_of_mut!((*self.regs).irq_vcpu).write_volatile(0) };
                }
                0
            }
            _ => u64::MAX,
        }
    }

    fn interrupt(&self, mem: &GuestMem, vcpus: &Vcpus, q: usize) {
        if self.queues[q].ready && self.queues[q].wants_interrupt(mem) {
            self.isr().fetch_or(abi::VIRTIO_ISR_VRING, Ordering::SeqCst);
            vcpus.raise(self.irq_vcpu(), self.event);
        }
    }

    /// The guest kicked: process queues whose kick bits are set (all
    /// queues in `extra`).
    pub fn poll(&mut self, mem: &GuestMem, vcpus: &Vcpus, extra: u64) {
        let bits = self.kick_word().swap(0, Ordering::SeqCst) | extra;
        if self.status() & STATUS_DRIVER_OK == 0 {
            return;
        }
        for q in 0..self.backend.num_queues() {
            if bits & (1 << q) != 0 && self.backend.kick(mem, &mut self.queues, q) {
                self.interrupt(mem, vcpus, q);
            }
        }
    }

    /// The backend signalled completions.
    pub fn complete(&mut self, mem: &GuestMem, vcpus: &Vcpus) {
        if self.backend.complete(mem, &mut self.queues) {
            for q in 0..self.backend.num_queues() {
                self.interrupt(mem, vcpus, q);
            }
        }
        // Completions free backend slots: retry requests left queued.
        self.poll(mem, vcpus, u64::MAX);
    }
}
