//! Uplink NIC: virtio-net PCI driver.  Frames are copied between the
//! switch and DMA buffers owned by this driver.

use alloc::vec::Vec;

use l4h_rt::host::net::{self, NicConfig};
use l4h_rt::sel4;
use l4h_virtio::{DESC_F_WRITE, Device, Dma, F_VERSION_1, Mmio, Virtq};

const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_HOST_TSO4: u64 = 1 << 11;
const VIRTIO_NET_F_HOST_TSO6: u64 = 1 << 12;
/// struct virtio_net_hdr_v1.
const HDR: usize = net::HDR_SIZE;
/// Receive buffers: MTU frames (no receive offloads).
const RX_BUF: usize = 2048;
/// Transmit buffers: whole TSO frames.
const TX_BUF: usize = HDR + net::MAX_FRAME;
const RX_QUEUE: u16 = 128;
const TX_BUFS: u16 = 24;

pub struct Nic {
    pub switch: Option<u32>,
    /// The device takes NEEDS_CSUM frames / TSO frames.
    pub csum: bool,
    pub tso: bool,
    rxq: Virtq,
    txq: Virtq,
    rx_bufs: Vec<(usize, u64)>,
    tx_bufs: Vec<(usize, u64)>,
    tx_free: Vec<u16>,
}

impl Nic {
    pub fn new(n: usize, c: &NicConfig) -> Self {
        let dev = Device {
            common: Mmio(c.common.va as usize),
            notify: Mmio(c.notify.va as usize),
            notify_mult: c.notify_off_multiplier,
            device: Mmio(c.device.va as usize),
        };
        let mut dma = Dma::new(net::nic_dma_va(n), c.dma_paddr, c.dma_size as usize);
        dev.reset();
        let offered = dev.features();
        let mut want = F_VERSION_1 | (offered & (VIRTIO_NET_F_MAC | VIRTIO_NET_F_CSUM));
        if want & VIRTIO_NET_F_CSUM != 0 {
            want |= offered & (VIRTIO_NET_F_HOST_TSO4 | VIRTIO_NET_F_HOST_TSO6);
        }
        if offered & F_VERSION_1 == 0 || !dev.set_features(want) {
            dev.fail();
            panic!("netd: NIC {} rejects VERSION_1", n);
        }
        let msix = Mmio(c.msix_table.va as usize);
        msix.w32(0, 0xfee0_0000);
        msix.w32(4, 0);
        msix.w32(8, c.msi_data);
        msix.w32(12, 0);
        let mut rxq = dev.setup_queue(&mut dma, 0, RX_QUEUE, 0).expect("netd: rx queue");
        let txq = dev.setup_queue(&mut dma, 1, RX_QUEUE, 0).expect("netd: tx queue");
        let rx_bufs: Vec<_> = (0..rxq.num).map(|_| dma.alloc(RX_BUF, 64)).collect();
        let tx_bufs: Vec<_> = (0..TX_BUFS.min(txq.num)).map(|_| dma.alloc(TX_BUF, 64)).collect();
        for (i, &(_, pa)) in rx_bufs.iter().enumerate() {
            rxq.set_desc(i as u16, pa, RX_BUF as u32, DESC_F_WRITE, 0);
            rxq.submit(i as u16);
        }
        let mac: [u8; 6] = core::array::from_fn(|i| dev.device.r8(i));
        dev.driver_ok();
        rxq.notify();
        sel4::debug_println!(
            "netd: NIC {}: {:02x}:{:02x}.{} mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            n, c.pci_bdf >> 8, (c.pci_bdf >> 3) & 0x1f, c.pci_bdf & 7,
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        let tx_free = (0..tx_bufs.len() as u16).rev().collect();
        Nic {
            switch: None,
            csum: want & VIRTIO_NET_F_CSUM != 0,
            tso: want & (VIRTIO_NET_F_HOST_TSO4 | VIRTIO_NET_F_HOST_TSO6) == (VIRTIO_NET_F_HOST_TSO4 | VIRTIO_NET_F_HOST_TSO6),
            rxq,
            txq,
            rx_bufs,
            tx_bufs,
            tx_free,
        }
    }

    /// Next received frame, copied to `out`; returns its length.
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        loop {
            let (id, len) = self.rxq.pop_used()?;
            let Some(&(va, _)) = self.rx_bufs.get(id as usize) else { continue };
            let len = len as usize;
            let n = len.saturating_sub(HDR).min(out.len());
            unsafe { core::ptr::copy_nonoverlapping((va + HDR) as *const u8, out.as_mut_ptr(), n) };
            // Give the buffer back to the device.
            self.rxq.submit(id as u16);
            self.rxq.notify();
            if len > HDR {
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
        if frame.len() + HDR > TX_BUF {
            return;
        }
        if self.tx_free.is_empty() {
            self.reap_tx();
        }
        let Some(i) = self.tx_free.pop() else { return };
        let (va, pa) = self.tx_bufs[i as usize];
        unsafe {
            core::ptr::copy_nonoverlapping(hdr.as_ptr(), va as *mut u8, HDR);
            // num_buffers is only meaningful on receive.
            core::ptr::write_bytes((va + 10) as *mut u8, 0, 2);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), (va + HDR) as *mut u8, frame.len());
        }
        self.txq.set_desc(i, pa, (HDR + frame.len()) as u32, 0, 0);
        self.txq.submit(i);
        self.txq.notify();
    }
}
