//! Uplink NICs: a virtio-net or gVNIC device, and the frames waiting for
//! one of its TX buffers.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use l4h_rt::host::net::{self, NicConfig};

use crate::gvnic::Gvnic;
use crate::virtio_net::VirtioNet;

const HDR: usize = net::HDR_SIZE;
/// Frames waiting for a TX buffer (rather than dropped), and their bytes.
const TX_BACKLOG_FRAMES: usize = 512;
const TX_BACKLOG_BYTES: usize = 4 << 20;

enum Dev {
    Virtio(VirtioNet),
    Gvnic(Gvnic),
}

/// Calls the same method on whichever device this is.
macro_rules! dev {
    ($self:expr, $d:ident => $e:expr) => {
        match &mut $self.dev {
            Dev::Virtio($d) => $e,
            Dev::Gvnic($d) => $e,
        }
    };
}

pub struct Nic {
    pub switch: Option<u32>,
    pub mac: [u8; 6],
    /// The device takes NEEDS_CSUM frames / TSO frames.
    pub csum: bool,
    pub tso: bool,
    dev: Dev,
    tx_backlog: VecDeque<(Vec<u8>, [u8; HDR])>,
    tx_backlog_bytes: usize,
    pub tx_drops: u64,
}

impl Nic {
    pub fn new(n: usize, c: &NicConfig, tsc_hz: u64) -> Self {
        let (dev, mac, csum, tso) = match c.kind {
            net::NIC_GVNIC => {
                let g = Gvnic::new(n, c, tsc_hz);
                let (mac, csum, tso) = (g.mac, g.csum, g.tso);
                (Dev::Gvnic(g), mac, csum, tso)
            }
            _ => {
                let v = VirtioNet::new(n, c);
                let (mac, csum, tso) = (v.mac, v.csum, v.tso);
                (Dev::Virtio(v), mac, csum, tso)
            }
        };
        Nic { switch: None, mac, csum, tso, dev, tx_backlog: VecDeque::new(), tx_backlog_bytes: 0, tx_drops: 0 }
    }

    /// Whether the device takes this GSO frame whole.
    pub fn takes_gso(&self, hdr: &[u8; HDR]) -> bool {
        match &self.dev {
            Dev::Virtio(_) => self.tso,
            Dev::Gvnic(g) => g.takes_gso(hdr),
        }
    }

    /// RX interrupts off while netd drains the queue.
    pub fn rx_begin(&mut self) {
        dev!(self, d => d.rx_begin())
    }

    /// RX interrupts on again; false if frames arrived meanwhile, which the
    /// caller drains (interrupts off again).  Returns buffers to the device
    /// with one notification per batch.
    pub fn rx_done(&mut self) -> bool {
        dev!(self, d => d.rx_done())
    }

    /// Next received frame, copied to `out` with its virtio-net header in
    /// `hdr` (num_buffers cleared); returns its length.
    pub fn receive(&mut self, out: &mut [u8], hdr: &mut [u8; HDR]) -> Option<usize> {
        dev!(self, d => d.receive(out, hdr))
    }

    /// Send a frame, or hold it until a TX buffer is free: a dropped frame
    /// costs TCP a retransmission timeout.
    pub fn send(&mut self, frame: &[u8], hdr: &[u8; HDR]) {
        if frame.len() > net::MAX_FRAME {
            self.tx_drops += 1;
            return;
        }
        if self.tx_backlog.is_empty() && dev!(self, d => d.transmit(frame, hdr)) {
            return;
        }
        if self.tx_backlog.len() >= TX_BACKLOG_FRAMES || self.tx_backlog_bytes + frame.len() > TX_BACKLOG_BYTES {
            self.tx_drops += 1;
            return;
        }
        self.tx_backlog.push_back((frame.to_vec(), *hdr));
        self.tx_backlog_bytes += frame.len();
        self.flush_tx();
    }

    /// Send held frames while TX buffers are free.  While some remain,
    /// the device interrupts when it returns buffers.
    pub fn flush_tx(&mut self) {
        loop {
            while let Some((frame, hdr)) = self.tx_backlog.pop_front() {
                if !dev!(self, d => d.transmit(&frame, &hdr)) {
                    self.tx_backlog.push_front((frame, hdr));
                    break;
                }
                self.tx_backlog_bytes -= frame.len();
            }
            let waiting = !self.tx_backlog.is_empty();
            // Buffers returned before the interrupt was on: send again.
            if !dev!(self, d => d.tx_interrupts(waiting)) || !waiting {
                return;
            }
        }
    }

    /// Notify the device of the frames submitted since the last call: once
    /// per batch, not per frame.
    pub fn kick_tx(&mut self) {
        dev!(self, d => d.kick_tx())
    }
}
