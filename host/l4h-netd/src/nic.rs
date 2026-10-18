//! Uplink NIC: virtio-net PCI driver, for modern (virtio 1.x) and legacy
//! (0.9.5, as on GCE) devices.  Frames are copied between the switch and
//! DMA buffers owned by this driver.

use alloc::vec::Vec;

use l4h_rt::host::net::{self, NicConfig};
use l4h_rt::sel4;
use l4h_virtio::{
    DESC_F_WRITE, Device, Dma, F_VERSION_1, Mmio, STATUS_ACK, STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FAILED, Virtq,
};

const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_HOST_TSO4: u64 = 1 << 11;
const VIRTIO_NET_F_HOST_TSO6: u64 = 1 << 12;
/// struct virtio_net_hdr_v1; legacy devices use the first 10 bytes
/// (struct virtio_net_hdr, without num_buffers).
const HDR: usize = net::HDR_SIZE;
const LEGACY_HDR: usize = 10;
/// Receive buffers: MTU frames (no receive offloads).
const RX_BUF: usize = 2048;
/// Transmit buffers: whole TSO frames.
const TX_BUF: usize = HDR + net::MAX_FRAME;
const RX_QUEUE: u16 = 128;
const TX_BUFS: u16 = 24;

/// Registers of a legacy virtio PCI device (I/O BAR 0), with MSI-X on.
mod legacy {
    pub const DEVICE_FEATURES: u16 = 0x00;
    pub const GUEST_FEATURES: u16 = 0x04;
    pub const QUEUE_PFN: u16 = 0x08;
    pub const QUEUE_SIZE: u16 = 0x0c;
    pub const QUEUE_SELECT: u16 = 0x0e;
    pub const QUEUE_NOTIFY: u16 = 0x10;
    pub const STATUS: u16 = 0x12;
    pub const CONFIG_VECTOR: u16 = 0x14;
    pub const QUEUE_VECTOR: u16 = 0x16;
    pub const DEVICE_CONFIG: u16 = 0x18;
}

/// A legacy device's I/O ports.
struct Ports {
    cap: u64,
    base: u16,
}

impl Ports {
    fn in8(&self, r: u16) -> u8 {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_In8(self.cap, self.base + r).result)
    }
    fn in16(&self, r: u16) -> u16 {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_In16(self.cap, self.base + r).result)
    }
    fn in32(&self, r: u16) -> u32 {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_In32(self.cap, self.base + r).result)
    }
    fn out8(&self, r: u16, v: u8) {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_Out8(self.cap, (self.base + r) as u64, v as u64));
    }
    fn out16(&self, r: u16, v: u16) {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_Out16(self.cap, (self.base + r) as u64, v as u64));
    }
    fn out32(&self, r: u16, v: u32) {
        sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_X86_IOPort_Out32(self.cap, (self.base + r) as u64, v as u64));
    }
}

pub struct Nic {
    pub switch: Option<u32>,
    pub mac: [u8; 6],
    /// The device takes NEEDS_CSUM frames / TSO frames.
    pub csum: bool,
    pub tso: bool,
    /// Legacy devices are notified through I/O ports.
    ports: Option<Ports>,
    /// Bytes of virtio-net header in front of each frame.
    hdr_len: usize,
    rxq: Virtq,
    txq: Virtq,
    rx_bufs: Vec<(usize, u64)>,
    tx_bufs: Vec<(usize, u64)>,
    tx_free: Vec<u16>,
}

/// Features this driver uses, of those the device offers.
fn wanted(offered: u64) -> u64 {
    let mut want = offered & (VIRTIO_NET_F_MAC | VIRTIO_NET_F_CSUM);
    if want & VIRTIO_NET_F_CSUM != 0 {
        want |= offered & (VIRTIO_NET_F_HOST_TSO4 | VIRTIO_NET_F_HOST_TSO6);
    }
    want
}

fn modern_device(c: &NicConfig) -> Device {
    Device {
        common: Mmio(c.common.va as usize),
        notify: Mmio(c.notify.va as usize),
        notify_mult: c.notify_off_multiplier,
        device: Mmio(c.device.va as usize),
    }
}

impl Nic {
    pub fn new(n: usize, c: &NicConfig) -> Self {
        let mut dma = Dma::new(net::nic_dma_va(n), c.dma_paddr, c.dma_size as usize);
        let msix = Mmio(c.msix_table.va as usize);
        msix.w32(0, 0xfee0_0000);
        msix.w32(4, 0);
        msix.w32(8, c.msi_data);
        msix.w32(12, 0);
        let (mut rxq, txq, want, mac, ports, hdr_len) = if c.legacy_io != 0 {
            let p = Ports { cap: net::NETD_SLOT_IOPORT_BASE + n as u64, base: c.legacy_io as u16 };
            p.out8(legacy::STATUS, 0);
            p.out8(legacy::STATUS, STATUS_ACK | STATUS_DRIVER);
            let want = wanted(p.in32(legacy::DEVICE_FEATURES) as u64);
            p.out32(legacy::GUEST_FEATURES, want as u32);
            p.out16(legacy::CONFIG_VECTOR, 0xffff);
            let mut queue = |idx: u16| {
                p.out16(legacy::QUEUE_SELECT, idx);
                let num = p.in16(legacy::QUEUE_SIZE);
                if num == 0 {
                    p.out8(legacy::STATUS, STATUS_FAILED);
                    panic!("netd: NIC {} has no queue {}", n, idx);
                }
                let (va, pa) = dma.alloc(l4h_virtio::legacy_ring_size(num), 4096);
                p.out32(legacy::QUEUE_PFN, (pa >> 12) as u32);
                p.out16(legacy::QUEUE_VECTOR, 0);
                if p.in16(legacy::QUEUE_VECTOR) != 0 {
                    panic!("netd: NIC {} refuses MSI-X vector for queue {}", n, idx);
                }
                Virtq::legacy(idx, num, va)
            };
            let (rxq, txq) = (queue(0), queue(1));
            let mac: [u8; 6] = core::array::from_fn(|i| p.in8(legacy::DEVICE_CONFIG + i as u16));
            (rxq, txq, want, mac, Some(p), LEGACY_HDR)
        } else {
            let dev = modern_device(c);
            dev.reset();
            let offered = dev.features();
            let want = F_VERSION_1 | wanted(offered);
            if offered & F_VERSION_1 == 0 || !dev.set_features(want) {
                dev.fail();
                panic!("netd: NIC {} rejects VERSION_1", n);
            }
            let rxq = dev.setup_queue(&mut dma, 0, RX_QUEUE, 0).expect("netd: rx queue");
            let txq = dev.setup_queue(&mut dma, 1, RX_QUEUE, 0).expect("netd: tx queue");
            let mac: [u8; 6] = core::array::from_fn(|i| dev.device.r8(i));
            (rxq, txq, want, mac, None, HDR)
        };
        // A legacy queue has the device's size: post at most RX_QUEUE.
        let rx_bufs: Vec<_> = (0..rxq.num.min(RX_QUEUE)).map(|_| dma.alloc(RX_BUF, 64)).collect();
        let tx_bufs: Vec<_> = (0..TX_BUFS.min(txq.num)).map(|_| dma.alloc(TX_BUF, 64)).collect();
        for (i, &(_, pa)) in rx_bufs.iter().enumerate() {
            rxq.set_desc(i as u16, pa, RX_BUF as u32, DESC_F_WRITE, 0);
            rxq.submit(i as u16);
        }
        let nic = Nic {
            switch: None,
            mac,
            csum: want & VIRTIO_NET_F_CSUM != 0,
            tso: want & (VIRTIO_NET_F_HOST_TSO4 | VIRTIO_NET_F_HOST_TSO6) == (VIRTIO_NET_F_HOST_TSO4 | VIRTIO_NET_F_HOST_TSO6),
            ports,
            hdr_len,
            rxq,
            txq,
            rx_bufs,
            tx_free: (0..tx_bufs.len() as u16).rev().collect(),
            tx_bufs,
        };
        match &nic.ports {
            Some(p) => p.out8(legacy::STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_DRIVER_OK),
            None => modern_device(c).driver_ok(),
        }
        nic.kick(&nic.rxq);
        sel4::debug_println!(
            "netd: NIC {}: {:02x}:{:02x}.{} {}, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            n, c.pci_bdf >> 8, (c.pci_bdf >> 3) & 0x1f, c.pci_bdf & 7,
            if nic.ports.is_some() { "legacy" } else { "modern" },
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        nic
    }

    fn kick(&self, q: &Virtq) {
        q.notify();
        if let Some(p) = &self.ports {
            p.out16(legacy::QUEUE_NOTIFY, q.idx);
        }
    }

    /// Next received frame, copied to `out`; returns its length.
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        let hl = self.hdr_len;
        loop {
            let (id, len) = self.rxq.pop_used()?;
            let Some(&(va, _)) = self.rx_bufs.get(id as usize) else { continue };
            let len = len as usize;
            let n = len.saturating_sub(hl).min(out.len());
            unsafe { core::ptr::copy_nonoverlapping((va + hl) as *const u8, out.as_mut_ptr(), n) };
            // Give the buffer back to the device.
            self.rxq.submit(id as u16);
            self.kick(&self.rxq);
            if len > hl {
                return Some(n);
            }
        }
    }

    pub fn reap_tx(&mut self) {
        while let Some((id, _)) = self.txq.pop_used() {
            self.tx_free.push(id as u16);
        }
    }

    pub fn transmit(&mut self, frame: &[u8], hdr: &[u8; HDR]) {
        let hl = self.hdr_len;
        if frame.len() + hl > TX_BUF {
            return;
        }
        if self.tx_free.is_empty() {
            self.reap_tx();
        }
        let Some(i) = self.tx_free.pop() else { return };
        let (va, pa) = self.tx_bufs[i as usize];
        unsafe {
            core::ptr::copy_nonoverlapping(hdr.as_ptr(), va as *mut u8, hl);
            if hl == HDR {
                // num_buffers is only meaningful on receive.
                core::ptr::write_bytes((va + 10) as *mut u8, 0, 2);
            }
            core::ptr::copy_nonoverlapping(frame.as_ptr(), (va + hl) as *mut u8, frame.len());
        }
        self.txq.set_desc(i, pa, (hl + frame.len()) as u32, 0, 0);
        self.txq.submit(i);
        self.kick(&self.txq);
    }
}
