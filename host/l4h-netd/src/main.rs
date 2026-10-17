//! netd: L2 virtual switches (DESIGN.md §8.3) and the uplink NIC drivers.
//!
//! Every port belongs to one switch.  A port is either a guest vNIC (a
//! channel to that guest's l4h-vmm) or an uplink NIC (virtio-net PCI,
//! driven here).  Switches learn source MACs, forward known unicast to one
//! port and flood everything else within the switch.  Guest ports can be
//! locked to their configured MAC.  Nothing above L2 is looked at.

#![no_std]
#![no_main]

extern crate alloc;

mod nic;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_rt::host::net::{self, NetdConfig};
use l4h_rt::sel4;
use l4h_rt::{Info, cap, host};
use sel4::cap_type;

use crate::nic::Nic;

/// Seconds after which a learned MAC is forgotten.
const FDB_AGE_SECS: u64 = 300;

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
    client: sel4::cap::Notification,
    /// The client must be told about new RX frames or freed TX slots.
    signal: bool,
    rx_frames: u64,
    tx_frames: u64,
    drops: u64,
}

#[derive(Default)]
struct Switch {
    fdb: BTreeMap<[u8; 6], (PortId, u64)>,
}

struct Netd {
    ports: Vec<Option<GuestPort>>,
    nics: Vec<Nic>,
    switches: BTreeMap<u32, Switch>,
    tsc_hz: u64,
}

fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi) };
    ((hi as u64) << 32) | lo as u64
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

    fn deliver(&mut self, to: PortId, frame: &[u8]) {
        match to {
            PortId::Guest(i) => {
                let Some(p) = self.ports[i].as_mut() else { return };
                unsafe {
                    let head = addr_of!((*p.chan).rx.head).read_volatile();
                    let tail = addr_of!((*p.chan).rx.tail).read_volatile();
                    if head.wrapping_sub(tail) as usize >= net::RING_SIZE {
                        p.drops += 1;
                        return;
                    }
                    let slot = addr_of_mut!((*p.chan).rx_slots[head as usize % net::RING_SIZE]);
                    core::ptr::copy_nonoverlapping(frame.as_ptr(), addr_of_mut!((*slot).data) as *mut u8, frame.len());
                    addr_of_mut!((*slot).len).write_volatile(frame.len() as u32);
                    fence(Ordering::Release);
                    addr_of_mut!((*p.chan).rx.head).write_volatile(head.wrapping_add(1));
                }
                p.rx_frames += 1;
                p.signal = true;
            }
            PortId::Uplink(n) => self.nics[n].transmit(frame),
        }
    }

    /// Switch one frame that arrived on port `from`.
    fn forward(&mut self, from: PortId, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        let Some(switch) = self.switch_of(from) else { return };
        let dst: [u8; 6] = frame[0..6].try_into().unwrap();
        let src: [u8; 6] = frame[6..12].try_into().unwrap();
        if let PortId::Guest(i) = from {
            let p = self.ports[i].as_mut().unwrap();
            if p.lock && src != p.mac {
                p.drops += 1;
                return;
            }
            p.tx_frames += 1;
        }
        let now = rdtsc();
        let age = FDB_AGE_SECS * self.tsc_hz;
        let sw = self.switches.entry(switch).or_default();
        if src[0] & 1 == 0 {
            sw.fdb.insert(src, (from, now));
        }
        let known = if dst[0] & 1 == 0 {
            match sw.fdb.get(&dst) {
                Some(&(port, seen)) if now.wrapping_sub(seen) < age => Some(port),
                Some(_) => {
                    sw.fdb.remove(&dst);
                    None
                }
                None => None,
            }
        } else {
            None
        };
        match known {
            Some(to) if to == from => {}
            Some(to) if self.switch_of(to) == Some(switch) => self.deliver(to, frame),
            _ => {
                for to in self.members(switch, from) {
                    self.deliver(to, frame);
                }
            }
        }
    }

    /// Take the frames a guest port has queued for transmission.
    fn port_tx(&mut self, i: usize) {
        let Some(p) = self.ports[i].as_ref() else { return };
        let chan = p.chan;
        let mut frame = [0u8; net::MAX_FRAME];
        loop {
            let n = unsafe {
                let tail = addr_of!((*chan).tx.tail).read_volatile();
                if tail == addr_of!((*chan).tx.head).read_volatile() {
                    break;
                }
                fence(Ordering::Acquire);
                let slot = addr_of!((*chan).tx_slots[tail as usize % net::RING_SIZE]);
                let n = (addr_of!((*slot).len).read_volatile() as usize).min(net::MAX_FRAME);
                core::ptr::copy_nonoverlapping(addr_of!((*slot).data) as *const u8, frame.as_mut_ptr(), n);
                fence(Ordering::Release);
                addr_of_mut!((*chan).tx.tail).write_volatile(tail.wrapping_add(1));
                n
            };
            self.forward(PortId::Guest(i), &frame[..n]);
        }
        if let Some(p) = self.ports[i].as_mut() {
            p.signal = true;
        }
    }

    fn nic_rx(&mut self, n: usize) {
        let mut frame = [0u8; net::MAX_FRAME];
        while let Some(len) = self.nics[n].receive(&mut frame) {
            if self.nics[n].switch.is_some() {
                self.forward(PortId::Uplink(n), &frame[..len]);
            }
        }
        self.nics[n].reap_tx();
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
        if self.ports.get_mut(i).and_then(|p| p.take()).is_some() {
            for sw in self.switches.values_mut() {
                sw.fdb.retain(|_, (p, _)| *p != PortId::Guest(i));
            }
        }
    }
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
        switches: BTreeMap::new(),
        tsc_hz: info.header().tsc_hz,
    };
    for (n, c) in cfg.nics[..cfg.nr_nics as usize].iter().enumerate() {
        d.nics.push(Nic::new(n, c));
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
                    }
                    d.ports[i] = Some(GuestPort {
                        chan,
                        switch: mr(1) as u32,
                        mac,
                        lock: mr(3) & net::NETD_PORT_MAC_LOCK != 0,
                        client: cap::<cap_type::Notification>(net::NETD_SLOT_CLIENT_BASE + i as u64),
                        signal: false,
                        rx_frames: 0,
                        tx_frames: 0,
                        drops: 0,
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

l4h_rt::component!(main, heap_size = 2 * 1024 * 1024);
