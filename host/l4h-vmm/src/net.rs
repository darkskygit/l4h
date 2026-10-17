//! virtio-net device backed by a netd port.  Frames are copied between
//! guest buffers and the port's slots.

use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_rt::host::net::{self, Port};
use l4h_rt::sel4;

use crate::dev::Backend;
use crate::guest::GuestMem;
use crate::virtq::{Buf, Queue};

const VIRTIO_ID_NET: u32 = 1;
const VIRTIO_NET_F_MTU: u64 = 1 << 3;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_S_LINK_UP: u16 = 1;
/// struct virtio_net_hdr_v1.
const HDR: usize = 12;
const RX: usize = 0;
const TX: usize = 1;
const QUEUE_SIZE: u32 = 256;

pub struct Net {
    chan: *mut Port,
    kick: sel4::cap::Notification,
    mac: [u8; 6],
    bufs: Vec<Buf>,
}

impl Net {
    pub fn new(chan_va: usize, kick: sel4::cap::Notification, mac: [u8; 6]) -> Self {
        Net { chan: chan_va as *mut Port, kick, mac, bufs: Vec::new() }
    }

    fn tx_full(&self) -> bool {
        unsafe {
            let head = addr_of!((*self.chan).tx.head).read_volatile();
            let tail = addr_of!((*self.chan).tx.tail).read_volatile();
            head.wrapping_sub(tail) as usize >= net::RING_SIZE
        }
    }

    /// Guest -> port.  Returns (returned to guest, queued to the port).
    fn transmit(&mut self, mem: &GuestMem, q: &mut Queue) -> (bool, bool) {
        let (mut returned, mut queued) = (false, false);
        loop {
            if self.tx_full() {
                break;
            }
            let Some(head) = q.pop(mem) else { break };
            let mut bufs = core::mem::take(&mut self.bufs);
            if q.chain(mem, head, &mut bufs).is_ok() {
                unsafe {
                    let h = addr_of!((*self.chan).tx.head).read_volatile();
                    let slot = addr_of_mut!((*self.chan).tx_slots[h as usize % net::RING_SIZE]);
                    let data = addr_of_mut!((*slot).data) as *mut u8;
                    // Skip the virtio-net header, gather the frame.
                    let (mut skip, mut n) = (HDR, 0usize);
                    let mut ok = true;
                    for b in bufs.iter().filter(|b| !b.write) {
                        let mut gpa = b.gpa;
                        let mut len = b.len as usize;
                        let s = skip.min(len);
                        skip -= s;
                        gpa += s as u64;
                        len -= s;
                        if n + len > net::MAX_FRAME {
                            ok = false;
                            break;
                        }
                        core::ptr::copy_nonoverlapping(mem.bytes(gpa, len), data.add(n), len);
                        n += len;
                    }
                    if ok && n >= 14 {
                        addr_of_mut!((*slot).len).write_volatile(n as u32);
                        fence(Ordering::Release);
                        addr_of_mut!((*self.chan).tx.head).write_volatile(h.wrapping_add(1));
                        queued = true;
                    }
                }
            }
            self.bufs = bufs;
            q.push(mem, head, 0);
            returned = true;
        }
        (returned, queued)
    }

    /// Port -> guest, while both have entries.
    fn receive(&mut self, mem: &GuestMem, q: &mut Queue) -> bool {
        let mut returned = false;
        loop {
            let (tail, head) = unsafe {
                (addr_of!((*self.chan).rx.tail).read_volatile(), addr_of!((*self.chan).rx.head).read_volatile())
            };
            if tail == head {
                break;
            }
            let Some(chain) = q.pop(mem) else { break };
            fence(Ordering::Acquire);
            let mut bufs = core::mem::take(&mut self.bufs);
            let mut written = 0usize;
            if q.chain(mem, chain, &mut bufs).is_ok() {
                unsafe {
                    let slot = addr_of!((*self.chan).rx_slots[tail as usize % net::RING_SIZE]);
                    let len = (addr_of!((*slot).len).read_volatile() as usize).min(net::MAX_FRAME);
                    let data = addr_of!((*slot).data) as *const u8;
                    // Header: all zero except num_buffers = 1.
                    let mut hdr = [0u8; HDR];
                    hdr[10] = 1;
                    let total = HDR + len;
                    for b in bufs.iter().filter(|b| b.write) {
                        let n = (b.len as usize).min(total - written);
                        let dst = mem.bytes(b.gpa, n);
                        for i in 0..n {
                            let k = written + i;
                            if k >= HDR {
                                // The rest is frame data: copy it at once.
                                core::ptr::copy_nonoverlapping(data.add(k - HDR), dst.add(i), n - i);
                                break;
                            }
                            *dst.add(i) = hdr[k];
                        }
                        written += n;
                        if written == total {
                            break;
                        }
                    }
                    if written < total {
                        // Buffers too small: drop the frame.
                        written = 0;
                    }
                }
            }
            self.bufs = bufs;
            unsafe {
                fence(Ordering::Release);
                addr_of_mut!((*self.chan).rx.tail).write_volatile(tail.wrapping_add(1));
            }
            q.push(mem, chain, written as u32);
            returned = true;
        }
        returned
    }
}

impl Backend for Net {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_NET
    }

    fn features(&self) -> u64 {
        VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS | VIRTIO_NET_F_MTU
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn queue_max(&self, _q: usize) -> u32 {
        QUEUE_SIZE
    }

    fn init_config(&self, config: &mut [u8]) {
        config[0..6].copy_from_slice(&self.mac);
        config[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
        config[10..12].copy_from_slice(&((net::MTU_FRAME - 14) as u16).to_le_bytes());
    }

    fn kick(&mut self, mem: &GuestMem, queues: &mut [Queue], q: usize) -> bool {
        match q {
            TX => {
                let (returned, queued) = self.transmit(mem, &mut queues[TX]);
                if queued {
                    self.kick.signal();
                }
                returned
            }
            RX => self.receive(mem, &mut queues[RX]),
            _ => false,
        }
    }

    fn complete(&mut self, mem: &GuestMem, queues: &mut [Queue]) -> bool {
        let mut returned = false;
        if queues[RX].ready {
            returned |= self.receive(mem, &mut queues[RX]);
        }
        if queues[TX].ready {
            let (r, queued) = self.transmit(mem, &mut queues[TX]);
            returned |= r;
            if queued {
                self.kick.signal();
            }
        }
        returned
    }

    fn reset(&mut self) {}
}
