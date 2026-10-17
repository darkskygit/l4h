//! PCI enumeration through the legacy configuration mechanism (I/O ports
//! 0xcf8/0xcfc), which every x86 platform l4h targets provides.

use alloc::vec::Vec;

use sel4::init_thread::slot;
use sel4::cap_type;

use crate::mem::Alloc;

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;

pub struct Pci {
    /// IOPort capability for 0xcf8..0xcff (rust-sel4 has no IOPort type).
    port: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Bar {
    pub addr: u64,
    pub size: u64,
    pub is_io: bool,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub bars: [Bar; 6],
    /// MSI-X capability: config offset, table BAR/offset, table size.
    pub msix: Option<Msix>,
    /// Vendor-specific capabilities (virtio): (cap offset, cfg_type, bar, offset, length, notify multiplier).
    pub virtio_caps: Vec<VirtioCap>,
}

#[derive(Clone, Copy, Debug)]
pub struct Msix {
    pub cap: u8,
    pub table_bar: u8,
    pub table_offset: u32,
    pub table_size: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct VirtioCap {
    pub cfg_type: u8,
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
    pub notify_off_multiplier: u32,
}

impl Pci {
    pub fn new(alloc: &mut Alloc) -> Self {
        let s = alloc.slots.alloc();
        let dst = slot::CNODE.cap().absolute_cptr(sel4::init_thread::Slot::<cap_type::Null>::from_index(s).cap());
        slot::IO_PORT_CONTROL
            .cap()
            .ioport_control_issue(CONFIG_ADDRESS as u64, CONFIG_DATA as u64 + 3, &dst)
            .expect("PCI config I/O ports");
        Pci { port: s as u64 }
    }

    fn addr(bus: u8, dev: u8, func: u8, off: u8) -> u64 {
        (1u64 << 31) | ((bus as u64) << 16) | ((dev as u64) << 11) | ((func as u64) << 8) | (off as u64 & 0xfc)
    }

    pub fn read32(&self, bus: u8, dev: u8, func: u8, off: u8) -> u32 {
        let port = self.port;
        sel4::with_ipc_buffer_mut(|ipc| {
            let ipc = ipc.inner_mut();
            ipc.seL4_X86_IOPort_Out32(port, CONFIG_ADDRESS as u64, Self::addr(bus, dev, func, off));
            ipc.seL4_X86_IOPort_In32(port, CONFIG_DATA).result
        })
    }

    pub fn write32(&self, bus: u8, dev: u8, func: u8, off: u8, val: u32) {
        let port = self.port;
        sel4::with_ipc_buffer_mut(|ipc| {
            let ipc = ipc.inner_mut();
            ipc.seL4_X86_IOPort_Out32(port, CONFIG_ADDRESS as u64, Self::addr(bus, dev, func, off));
            ipc.seL4_X86_IOPort_Out32(port, CONFIG_DATA as u64, val as u64);
        });
    }

    pub fn read16(&self, bus: u8, dev: u8, func: u8, off: u8) -> u16 {
        (self.read32(bus, dev, func, off & !3) >> ((off & 2) * 8)) as u16
    }

    pub fn write16(&self, bus: u8, dev: u8, func: u8, off: u8, val: u16) {
        let old = self.read32(bus, dev, func, off & !3);
        let shift = (off & 2) * 8;
        let new = (old & !(0xffff << shift)) | ((val as u32) << shift);
        self.write32(bus, dev, func, off & !3, new);
    }

    fn read8(&self, bus: u8, dev: u8, func: u8, off: u8) -> u8 {
        (self.read32(bus, dev, func, off & !3) >> ((off & 3) * 8)) as u8
    }

    fn probe_bars(&self, bus: u8, dev: u8, func: u8) -> [Bar; 6] {
        let mut bars = [Bar::default(); 6];
        let cmd = self.read16(bus, dev, func, 4);
        // Disable decoding while sizing.
        self.write16(bus, dev, func, 4, cmd & !3);
        let mut i = 0;
        while i < 6 {
            let off = 0x10 + 4 * i as u8;
            let orig = self.read32(bus, dev, func, off);
            if orig & 1 != 0 {
                self.write32(bus, dev, func, off, 0xffff_ffff);
                let sz = self.read32(bus, dev, func, off) & !3;
                self.write32(bus, dev, func, off, orig);
                if sz != 0 {
                    bars[i] = Bar { addr: (orig & !3) as u64, size: ((!sz).wrapping_add(1) & 0xffff) as u64, is_io: true };
                }
                i += 1;
                continue;
            }
            let is64 = (orig >> 1) & 3 == 2;
            self.write32(bus, dev, func, off, 0xffff_ffff);
            let lo = self.read32(bus, dev, func, off) & !0xf;
            self.write32(bus, dev, func, off, orig);
            let mut addr = (orig & !0xf) as u64;
            let mut mask = lo as u64 | 0xffff_ffff_0000_0000;
            if is64 {
                let orig_hi = self.read32(bus, dev, func, off + 4);
                self.write32(bus, dev, func, off + 4, 0xffff_ffff);
                let hi = self.read32(bus, dev, func, off + 4);
                self.write32(bus, dev, func, off + 4, orig_hi);
                addr |= (orig_hi as u64) << 32;
                mask = ((hi as u64) << 32) | lo as u64;
            }
            if lo != 0 {
                bars[i] = Bar { addr, size: (!mask).wrapping_add(1), is_io: false };
            }
            i += if is64 { 2 } else { 1 };
        }
        self.write16(bus, dev, func, 4, cmd);
        bars
    }

    fn caps(&self, bus: u8, dev: u8, func: u8) -> (Option<Msix>, Vec<VirtioCap>) {
        let mut msix = None;
        let mut virtio = Vec::new();
        if self.read16(bus, dev, func, 6) & 0x10 == 0 {
            return (msix, virtio);
        }
        let mut ptr = self.read8(bus, dev, func, 0x34) & !3;
        let mut guard = 0;
        while ptr != 0 && guard < 48 {
            let id = self.read8(bus, dev, func, ptr);
            match id {
                0x11 => {
                    let ctrl = self.read16(bus, dev, func, ptr + 2);
                    let table = self.read32(bus, dev, func, ptr + 4);
                    msix = Some(Msix {
                        cap: ptr,
                        table_bar: (table & 7) as u8,
                        table_offset: table & !7,
                        table_size: (ctrl & 0x7ff) + 1,
                    });
                }
                0x09 => {
                    let cfg_type = self.read8(bus, dev, func, ptr + 3);
                    let bar = self.read8(bus, dev, func, ptr + 4);
                    let offset = self.read32(bus, dev, func, ptr + 8);
                    let length = self.read32(bus, dev, func, ptr + 12);
                    let mult = if cfg_type == 2 { self.read32(bus, dev, func, ptr + 16) } else { 0 };
                    virtio.push(VirtioCap { cfg_type, bar, offset, length, notify_off_multiplier: mult });
                }
                _ => {}
            }
            ptr = self.read8(bus, dev, func, ptr + 1) & !3;
            guard += 1;
        }
        (msix, virtio)
    }

    pub fn enumerate(&self) -> Vec<Device> {
        let mut out = Vec::new();
        for bus in 0..=255u8 {
            for dev in 0..32u8 {
                let id = self.read32(bus, dev, 0, 0);
                if id & 0xffff == 0xffff {
                    continue;
                }
                let multi = self.read8(bus, dev, 0, 0xe) & 0x80 != 0;
                for func in 0..if multi { 8 } else { 1 } {
                    let id = self.read32(bus, dev, func, 0);
                    if id & 0xffff == 0xffff {
                        continue;
                    }
                    let class = self.read32(bus, dev, func, 8);
                    let header = self.read8(bus, dev, func, 0xe) & 0x7f;
                    let (msix, virtio_caps) = self.caps(bus, dev, func);
                    let bars = if header == 0 { self.probe_bars(bus, dev, func) } else { [Bar::default(); 6] };
                    out.push(Device {
                        bus,
                        dev,
                        func,
                        vendor: id as u16,
                        device: (id >> 16) as u16,
                        class: (class >> 24) as u8,
                        subclass: (class >> 16) as u8,
                        prog_if: (class >> 8) as u8,
                        bars,
                        msix,
                        virtio_caps,
                    });
                }
            }
        }
        out
    }

    /// Enable memory decoding and bus mastering, disable INTx.
    pub fn enable(&self, d: &Device) {
        let cmd = self.read16(d.bus, d.dev, d.func, 4);
        self.write16(d.bus, d.dev, d.func, 4, cmd | 0x2 | 0x4 | 0x400);
    }

    /// Enable MSI-X with all vectors unmasked at the function level.
    pub fn enable_msix(&self, d: &Device) {
        if let Some(m) = d.msix {
            let ctrl = self.read16(d.bus, d.dev, d.func, m.cap + 2);
            self.write16(d.bus, d.dev, d.func, m.cap + 2, (ctrl | 0x8000) & !0x4000);
        }
    }
}
