//! netd: L2 virtual switches (DESIGN.md §8.3) and the uplink NIC drivers.
//!
//! Every port belongs to one switch.  A port is either a guest vNIC (a
//! channel to that guest's l4h-vmm) or an uplink NIC (virtio-net or gVNIC
//! PCI, driven here).  Switches learn source MACs, forward known unicast to one
//! port and flood everything else within the switch.  Guest ports can be
//! locked to their configured MAC.  Nothing above L2 is looked at.

#![no_std]
#![no_main]

extern crate alloc;

mod gvnic;
mod nic;
mod offload;
mod virtio_net;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_rt::host::net::{self, NetdConfig};
use l4h_rt::sel4;
use l4h_rt::{Info, cap, host};
use sel4::cap_type;

use crate::nic::Nic;
use crate::offload::{complete_csum, is_gso};

/// Seconds after which a learned MAC is forgotten.
const FDB_AGE_SECS: u64 = 300;
/// Learned MACs per switch and in all.  When a table is full, expired
/// entries go; if none has, new sources are not learned (their frames are
/// still switched, by flooding).
const FDB_MAX: usize = 1024;
const FDB_MAX_TOTAL: usize = 8192;
/// Frames held for a guest whose RX ring is full, per port and in all.
/// Dropping them instead costs TCP a retransmission timeout whenever a
/// burst outruns the ring (24 slots); a guest that stops reading costs
/// only its own backlog.
const BACKLOG_FRAMES: usize = 512;
const BACKLOG_BYTES: usize = 2 << 20;
const BACKLOG_BYTES_TOTAL: usize = 16 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PortId {
    Guest(usize),
    Uplink(usize),
}

struct GuestPort {
    chan: *mut net::Port,
    switch: u32,
    mac: [u8; 6],
    lock: bool,
    /// An isolated port exchanges frames only with ports that are not
    /// isolated (other guests' normal ports and the uplink).
    isolated: bool,
    client: sel4::cap::Notification,
    /// The client must be told about new RX frames or freed TX slots.
    signal: bool,
    rx_frames: u64,
    tx_frames: u64,
    drops: u64,
    /// Frames for a full RX ring, oldest first, and their bytes.
    backlog: VecDeque<(Vec<u8>, [u8; net::HDR_SIZE])>,
    backlog_bytes: usize,
}

#[derive(Default)]
struct Switch {
    fdb: BTreeMap<[u8; 6], (PortId, u64)>,
}

struct Netd {
    ports: Vec<Option<GuestPort>>,
    nics: Vec<Nic>,
    /// Receive offload per NIC (taken out while it emits).
    gro: Vec<Option<offload::Gro>>,
    switches: BTreeMap<u32, Switch>,
    tsc_hz: u64,
    /// Frame being switched.
    buf: Vec<u8>,
    /// Segments of a GSO frame for a port that cannot take it whole.
    seg_buf: Vec<u8>,
    /// Entries in all forwarding tables.
    fdb_total: usize,
    /// Bytes in all ports' backlogs.
    backlog_total: usize,
}

fn rdtsc() -> u64 {
    host::ticks::now()
}

fn mac_of(v: u64) -> [u8; 6] {
    let b = v.to_be_bytes();
    [b[2], b[3], b[4], b[5], b[6], b[7]]
}

impl Netd {
    fn switch_of(&self, p: PortId) -> Option<u32> {
        match p {
            PortId::Guest(i) => self.ports[i].as_ref().map(|g| g.switch),
            PortId::Uplink(n) => self.nics[n].switch,
        }
    }

    /// Ports of `switch` other than `from`.
    fn members(&self, switch: u32, from: PortId) -> Vec<PortId> {
        let mut v: Vec<PortId> = self
            .ports
            .iter()
            .enumerate()
            .filter(|(_, p)| p.as_ref().is_some_and(|p| p.switch == switch))
            .map(|(i, _)| PortId::Guest(i))
            .collect();
        v.extend(self.nics.iter().enumerate().filter(|(_, n)| n.switch == Some(switch)).map(|(i, _)| PortId::Uplink(i)));
        v.retain(|&p| p != from);
        v
    }

    /// Whether port `to` takes this GSO frame whole.
    fn takes_gso(&self, to: PortId, hdr: &[u8; net::HDR_SIZE]) -> bool {
        match to {
            PortId::Guest(i) => self.ports[i]
                .as_ref()
                .is_some_and(|p| unsafe { addr_of!((*p.chan).rx_gso).read_volatile() != 0 }),
            PortId::Uplink(n) => self.nics[n].takes_gso(hdr),
        }
    }

    fn deliver(&mut self, to: PortId, frame: &mut [u8], hdr: &mut [u8; net::HDR_SIZE]) {
        if is_gso(hdr) && !self.takes_gso(to, hdr) {
            let mut buf = core::mem::take(&mut self.seg_buf);
            offload::segment(frame, hdr, &mut buf, |f, h| self.deliver_one(to, f, h));
            self.seg_buf = buf;
            return;
        }
        self.deliver_one(to, frame, hdr);
    }

    fn deliver_one(&mut self, to: PortId, frame: &mut [u8], hdr: &mut [u8; net::HDR_SIZE]) {
        match to {
            PortId::Guest(i) => {
                let Some(p) = self.ports[i].as_mut() else { return };
                let rx_csum = unsafe { addr_of!((*p.chan).rx_csum).read_volatile() != 0 };
                if !rx_csum {
                    complete_csum(frame, hdr);
                    hdr[0] = 0;
                }
                // Behind a backlog, or no room: hold the frame (in order).
                if p.backlog.is_empty() && rx_put(p.chan, frame, hdr) {
                    p.rx_frames += 1;
                    p.signal = true;
                } else {
                    if p.backlog.len() >= BACKLOG_FRAMES
                        || p.backlog_bytes + frame.len() > BACKLOG_BYTES
                        || self.backlog_total + frame.len() > BACKLOG_BYTES_TOTAL
                    {
                        p.drops += 1;
                        return;
                    }
                    p.backlog.push_back((frame.to_vec(), *hdr));
                    p.backlog_bytes += frame.len();
                    self.backlog_total += frame.len();
                    self.rx_backlog(i);
                }
            }
            PortId::Uplink(n) => {
                let nic = &mut self.nics[n];
                if !nic.csum {
                    complete_csum(frame, hdr);
                }
                nic.send(frame, hdr);
            }
        }
    }

    /// Switch one frame that arrived on port `from`.
    fn forward(&mut self, from: PortId, frame: &mut [u8], hdr: &mut [u8; net::HDR_SIZE]) {
        if frame.len() < 14 {
            return;
        }
        let Some(switch) = self.switch_of(from) else { return };
        let dst: [u8; 6] = frame[0..6].try_into().unwrap();
        let src: [u8; 6] = frame[6..12].try_into().unwrap();
        if let PortId::Guest(i) = from {
            let p = self.ports[i].as_mut().unwrap();
            if (p.lock && src != p.mac) || !offload::sanitize(hdr, frame) {
                p.drops += 1;
                return;
            }
            p.tx_frames += 1;
        }
        let now = rdtsc();
        let age = FDB_AGE_SECS * self.tsc_hz;
        let sw = self.switches.entry(switch).or_default();
        if src[0] & 1 == 0 {
            if let Some(e) = sw.fdb.get_mut(&src) {
                *e = (from, now);
            } else {
                if sw.fdb.len() >= FDB_MAX || self.fdb_total >= FDB_MAX_TOTAL {
                    let before = sw.fdb.len();
                    sw.fdb.retain(|_, &mut (_, seen)| now.wrapping_sub(seen) < age);
                    self.fdb_total -= before - sw.fdb.len();
                }
                if sw.fdb.len() < FDB_MAX && self.fdb_total < FDB_MAX_TOTAL {
                    sw.fdb.insert(src, (from, now));
                    self.fdb_total += 1;
                }
            }
        }
        let known = if dst[0] & 1 == 0 {
            match sw.fdb.get(&dst) {
                Some(&(port, seen)) if now.wrapping_sub(seen) < age => Some(port),
                Some(_) => {
                    sw.fdb.remove(&dst);
                    self.fdb_total -= 1;
                    None
                }
                None => None,
            }
        } else {
            None
        };
        let from_isolated = self.is_isolated(from);
        match known {
            Some(to) if to == from => {}
            Some(to) if from_isolated && self.is_isolated(to) => {}
            Some(to) if self.switch_of(to) == Some(switch) => self.deliver(to, frame, hdr),
            _ => {
                for to in self.members(switch, from) {
                    if !(from_isolated && self.is_isolated(to)) {
                        self.deliver(to, frame, hdr);
                    }
                }
            }
        }
    }

    fn is_isolated(&self, p: PortId) -> bool {
        match p {
            PortId::Guest(i) => self.ports[i].as_ref().is_some_and(|p| p.isolated),
            PortId::Uplink(_) => false,
        }
    }

    /// Move port `i`'s backlog into its RX ring while there is room.  While
    /// frames remain, the client is asked to kick netd when it frees slots.
    fn rx_backlog(&mut self, i: usize) {
        let Some(p) = self.ports[i].as_mut() else { return };
        loop {
            while let Some((frame, hdr)) = p.backlog.front() {
                if !rx_put(p.chan, frame, hdr) {
                    break;
                }
                let n = frame.len();
                p.backlog.pop_front();
                p.backlog_bytes -= n;
                self.backlog_total -= n;
                p.rx_frames += 1;
                p.signal = true;
            }
            let wanted = !p.backlog.is_empty();
            unsafe { addr_of_mut!((*p.chan).rx_wanted).write_volatile(wanted as u32) };
            if !wanted {
                return;
            }
            // The client may have freed slots before it saw rx_wanted.
            fence(Ordering::SeqCst);
            if rx_full(p.chan) {
                return;
            }
        }
    }

    /// Take the frames a guest port has queued for transmission.
    fn port_tx(&mut self, i: usize) {
        let Some(p) = self.ports[i].as_ref() else { return };
        let chan = p.chan;
        let mut frame = core::mem::take(&mut self.buf);
        // The indices are the client's: at most one ring's worth per kick,
        // and nothing at all from a ring that claims more.
        let pending = unsafe { addr_of!((*chan).tx.head).read_volatile().wrapping_sub(addr_of!((*chan).tx.tail).read_volatile()) };
        if pending as usize > net::RING_SIZE {
            if let Some(p) = self.ports[i].as_mut() {
                p.drops += 1;
            }
            self.buf = frame;
            return;
        }
        for _ in 0..pending {
            let (n, mut hdr) = unsafe {
                let tail = addr_of!((*chan).tx.tail).read_volatile();
                if tail == addr_of!((*chan).tx.head).read_volatile() {
                    break;
                }
                fence(Ordering::Acquire);
                let slot = addr_of!((*chan).tx_slots[tail as usize % net::RING_SIZE]);
                let n = (addr_of!((*slot).len).read_volatile() as usize).min(net::MAX_FRAME);
                core::ptr::copy_nonoverlapping(addr_of!((*slot).data) as *const u8, frame.as_mut_ptr(), n);
                let hdr = addr_of!((*slot).hdr).read_volatile();
                fence(Ordering::Release);
                addr_of_mut!((*chan).tx.tail).write_volatile(tail.wrapping_add(1));
                (n, hdr)
            };
            self.forward(PortId::Guest(i), &mut frame[..n], &mut hdr);
        }
        self.buf = frame;
        if let Some(p) = self.ports[i].as_mut() {
            p.signal = true;
        }
    }

    fn nic_rx(&mut self, n: usize) {
        let mut frame = core::mem::take(&mut self.buf);
        self.nics[n].rx_begin();
        loop {
            let mut hdr = [0u8; net::HDR_SIZE];
            while let Some(len) = self.nics[n].receive(&mut frame, &mut hdr) {
                if self.nics[n].switch.is_none() {
                    continue;
                }
                if offload::sanitize_rx(&mut hdr, &frame[..len]) {
                    // Merge TCP segments into one GSO frame where possible.
                    if self.gro[n].as_mut().unwrap().add(&frame[..len], &hdr) {
                        continue;
                    }
                    self.gro_flush(n);
                    if self.gro[n].as_mut().unwrap().add(&frame[..len], &hdr) {
                        continue;
                    }
                    self.forward(PortId::Uplink(n), &mut frame[..len], &mut hdr);
                } else if is_gso(&hdr) {
                    // A large frame the device checked but did not leave
                    // a partial checksum in: forward its segments.
                    let mut buf = core::mem::take(&mut self.seg_buf);
                    offload::segment(&frame[..len], &hdr, &mut buf, |f, h| self.forward(PortId::Uplink(n), f, h));
                    self.seg_buf = buf;
                }
            }
            self.gro_flush(n);
            if self.nics[n].rx_done() {
                break;
            }
        }
        self.buf = frame;
        self.nics[n].flush_tx();
    }

    /// Forward the TCP run NIC `n`'s receive offload has built.
    fn gro_flush(&mut self, n: usize) {
        let Some(mut g) = self.gro[n].take() else { return };
        g.flush(|f, h| self.forward(PortId::Uplink(n), f, h));
        self.gro[n] = Some(g);
    }

    fn signal_clients(&mut self) {
        for p in self.ports.iter_mut().flatten() {
            if p.signal {
                p.signal = false;
                p.client.signal();
            }
        }
    }

    fn remove_port(&mut self, i: usize) {
        if let Some(p) = self.ports.get_mut(i).and_then(|p| p.take()) {
            self.backlog_total -= p.backlog_bytes;
            for sw in self.switches.values_mut() {
                let before = sw.fdb.len();
                sw.fdb.retain(|_, (p, _)| *p != PortId::Guest(i));
                self.fdb_total -= before - sw.fdb.len();
            }
        }
    }
}

fn rx_full(chan: *mut net::Port) -> bool {
    unsafe {
        let head = addr_of!((*chan).rx.head).read_volatile();
        let tail = addr_of!((*chan).rx.tail).read_volatile();
        head.wrapping_sub(tail) as usize >= net::RING_SIZE
    }
}

/// Put a frame into a port's RX ring; false if it is full.
fn rx_put(chan: *mut net::Port, frame: &[u8], hdr: &[u8; net::HDR_SIZE]) -> bool {
    if rx_full(chan) {
        return false;
    }
    unsafe {
        let head = addr_of!((*chan).rx.head).read_volatile();
        let slot = addr_of_mut!((*chan).rx_slots[head as usize % net::RING_SIZE]);
        core::ptr::copy_nonoverlapping(frame.as_ptr(), addr_of_mut!((*slot).data) as *mut u8, frame.len());
        addr_of_mut!((*slot).hdr).write_volatile(*hdr);
        addr_of_mut!((*slot).len).write_volatile(frame.len() as u32);
        fence(Ordering::Release);
        addr_of_mut!((*chan).rx.head).write_volatile(head.wrapping_add(1));
    }
    true
}

fn mr(i: usize) -> u64 {
    sel4::with_ipc_buffer(|b| b.msg_regs()[i])
}

fn set_mr(i: usize, v: u64) {
    sel4::with_ipc_buffer_mut(|b| b.msg_regs_mut()[i] = v)
}

fn main(info: &'static Info) -> ! {
    let cfg: &NetdConfig = unsafe { info.config() };
    let mut d = Netd {
        ports: (0..net::MAX_PORTS).map(|_| None).collect(),
        nics: Vec::new(),
        gro: Vec::new(),
        switches: BTreeMap::new(),
        tsc_hz: info.header().tsc_hz,
        buf: alloc::vec![0u8; net::MAX_FRAME],
        seg_buf: alloc::vec![0u8; net::MAX_FRAME],
        fdb_total: 0,
        backlog_total: 0,
    };
    for (n, c) in cfg.nics[..cfg.nr_nics as usize].iter().enumerate() {
        d.nics.push(Nic::new(n, c, d.tsc_hz));
        d.gro.push(Some(offload::Gro::new()));
    }
    let ep = cap::<cap_type::Endpoint>(host::SLOT_SERVICE_EP);
    let reply = cap::<cap_type::Reply>(host::SLOT_REPLY);
    sel4::debug_println!("netd: {} uplink NIC(s)", d.nics.len());

    let (mut msg, mut badge) = ep.recv(reply);
    loop {
        if badge != 0 {
            // Notification: port kicks and NIC interrupts.
            for i in 0..net::MAX_PORTS {
                if badge & (1 << i) != 0 {
                    d.port_tx(i);
                    d.rx_backlog(i);
                }
            }
            for n in 0..d.nics.len() {
                if badge & (1 << (net::NETD_BADGE_IRQ_SHIFT as usize + n)) != 0 {
                    d.nic_rx(n);
                    cap::<cap_type::IrqHandler>(net::NETD_SLOT_IRQ_BASE + n as u64)
                        .irq_handler_ack()
                        .expect("netd: IRQ ack");
                }
            }
            for nic in &mut d.nics {
                nic.kick_tx();
            }
            d.signal_clients();
            (msg, badge) = ep.recv(reply);
            continue;
        }
        // A call from l4h-root.
        let resp = match msg.label() {
            net::NETD_ADD_PORT => {
                let i = mr(0) as usize;
                if i < net::MAX_PORTS {
                    d.remove_port(i);
                    let chan = net::port_va(i) as *mut net::Port;
                    let mac = mac_of(mr(2));
                    unsafe {
                        addr_of_mut!((*chan).mac).write_volatile([mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], 0, 0]);
                        addr_of_mut!((*chan).link_up).write_volatile(1);
                        addr_of_mut!((*chan).rx_wanted).write_volatile(0);
                    }
                    d.ports[i] = Some(GuestPort {
                        chan,
                        switch: mr(1) as u32,
                        mac,
                        lock: mr(3) & net::NETD_PORT_MAC_LOCK != 0,
                        isolated: mr(3) & net::NETD_PORT_ISOLATED != 0,
                        client: cap::<cap_type::Notification>(net::NETD_SLOT_CLIENT_BASE + i as u64),
                        signal: false,
                        rx_frames: 0,
                        tx_frames: 0,
                        drops: 0,
                        backlog: VecDeque::new(),
                        backlog_bytes: 0,
                    });
                    0
                } else {
                    u64::MAX
                }
            }
            net::NETD_REMOVE_PORT => {
                d.remove_port(mr(0) as usize);
                0
            }
            net::NETD_ATTACH_UPLINK => {
                let (n, sw) = (mr(0) as usize, mr(1) as u32);
                match d.nics.get_mut(n) {
                    Some(nic) => {
                        nic.switch = Some(sw);
                        0
                    }
                    None => u64::MAX,
                }
            }
            net::NETD_NIC_MAC => match d.nics.get(mr(0) as usize) {
                Some(n) => n.mac.iter().fold(0u64, |m, &b| (m << 8) | b as u64),
                None => u64::MAX,
            },
            net::NETD_PORT_STATS => match d.ports.get(mr(0) as usize).and_then(|p| p.as_ref()) {
                Some(p) => {
                    set_mr(1, p.tx_frames);
                    set_mr(2, p.drops);
                    p.rx_frames
                }
                None => u64::MAX,
            },
            _ => u64::MAX,
        };
        set_mr(0, resp);
        (msg, badge) = ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, 3), reply);
    }
}

// Room for the forwarding tables and BACKLOG_BYTES_TOTAL of held frames.
l4h_rt::component!(main, heap_size = 24 * 1024 * 1024);
