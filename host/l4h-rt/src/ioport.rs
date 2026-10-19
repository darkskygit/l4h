//! x86 I/O port access through an seL4 IOPort capability, for legacy
//! (0.9.5) virtio PCI devices, whose registers are in I/O BAR 0.

/// Ports [base, ...) reachable through the IOPort capability in `cap`.
pub struct Ports {
    pub cap: u64,
    pub base: u16,
}

#[cfg(target_arch = "x86_64")]
impl Ports {
    pub fn in8(&self, r: u16) -> u8 {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_In8(self.cap, self.base + r).result)
    }
    pub fn in16(&self, r: u16) -> u16 {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_In16(self.cap, self.base + r).result)
    }
    pub fn in32(&self, r: u16) -> u32 {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_In32(self.cap, self.base + r).result)
    }
    pub fn out8(&self, r: u16, v: u8) {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_Out8(self.cap, (self.base + r) as u64, v as u64));
    }
    pub fn out16(&self, r: u16, v: u16) {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_Out16(self.cap, (self.base + r) as u64, v as u64));
    }
    pub fn out32(&self, r: u16, v: u32) {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_Out32(self.cap, (self.base + r) as u64, v as u64));
    }
}

/// Legacy (I/O port) virtio devices exist only on x86 (GCE N2).
#[cfg(not(target_arch = "x86_64"))]
impl Ports {
    pub fn in8(&self, _r: u16) -> u8 {
        unreachable!("I/O ports at {:#x} (cap {})", self.base, self.cap)
    }
    pub fn in16(&self, _r: u16) -> u16 {
        unreachable!("I/O ports")
    }
    pub fn in32(&self, _r: u16) -> u32 {
        unreachable!("I/O ports")
    }
    pub fn out8(&self, _r: u16, _v: u8) {
        unreachable!("I/O ports")
    }
    pub fn out16(&self, _r: u16, _v: u16) {
        unreachable!("I/O ports")
    }
    pub fn out32(&self, _r: u16, _v: u32) {
        unreachable!("I/O ports")
    }
}
