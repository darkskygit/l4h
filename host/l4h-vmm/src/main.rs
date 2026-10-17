//! l4h-vmm: the per-guest virtual machine monitor.  Serves the guest's
//! hypercalls and implements its virtio-l4h devices on top of the host
//! drivers (blkd, netd).  See DESIGN.md §8.

#![no_std]
#![no_main]

extern crate alloc;

mod blk;
mod dev;
mod guest;
mod net;
mod virtq;

use alloc::vec::Vec;

use l4h_abi as abi;
use l4h_rt::host::{self, VmmConfig};
use l4h_rt::sel4;
use l4h_rt::{Info, cap};
use sel4::cap_type;

use crate::dev::Device;
use crate::guest::{GuestMem, Vcpus};

enum Dev {
    Blk(Device<blk::Blk>),
    Net(Device<net::Net>),
}

impl Dev {
    fn op(&mut self, mem: &GuestMem, vcpus: &Vcpus, op: u64, a: u64, b: u64) -> u64 {
        match self {
            Dev::Blk(d) => d.op(mem, vcpus, op, a, b),
            Dev::Net(d) => d.op(mem, vcpus, op, a, b),
        }
    }

    fn poll(&mut self, mem: &GuestMem, vcpus: &Vcpus) {
        match self {
            Dev::Blk(d) => d.poll(mem, vcpus, 0),
            Dev::Net(d) => d.poll(mem, vcpus, 0),
        }
    }

    fn complete(&mut self, mem: &GuestMem, vcpus: &Vcpus) {
        match self {
            Dev::Blk(d) => d.complete(mem, vcpus),
            Dev::Net(d) => d.complete(mem, vcpus),
        }
    }
}

/// Guest console output: into the ring l4h-root reads, and optionally to
/// the serial port, each line prefixed with the guest id (except dom0).
struct Console {
    ring: *mut host::ConsoleRing,
    serial: bool,
    prefix: Option<u32>,
    line_start: bool,
}

impl Console {
    fn put(&mut self, c: u8) {
        unsafe {
            let head = core::ptr::addr_of!((*self.ring).head).read_volatile();
            core::ptr::addr_of_mut!((*self.ring).data[head as usize % host::CONSOLE_RING_DATA]).write_volatile(c);
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            core::ptr::addr_of_mut!((*self.ring).head).write_volatile(head + 1);
        }
        if self.serial {
            if self.line_start {
                if let Some(id) = self.prefix {
                    sel4::debug_print!("[vm{}] ", id);
                }
            }
            sel4::debug_put_char(c);
            self.line_start = c == b'\n';
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
    let cfg: &VmmConfig = unsafe { info.config() };
    let mem = GuestMem { size: cfg.ram_size };
    let vcpus = Vcpus { n: cfg.nr_vcpus as usize };

    // Block backends announce themselves ready (capacity known) on the
    // channel and signal our notification.
    let ntfn = cap::<cap_type::Notification>(host::SLOT_NTFN);
    let devices = &cfg.devices[..cfg.nr_devices as usize];
    while !devices
        .iter()
        .filter(|d| d.kind == host::VDEV_BLK)
        .all(|d| unsafe { core::ptr::addr_of!((*(d.chan_va as *const host::blk::Channel)).ready).read_volatile() } != 0)
    {
        ntfn.wait();
    }

    // Devices by guest boot info index.
    let mut devs: Vec<Option<Dev>> = Vec::new();
    let mut by_slot: Vec<usize> = Vec::new();
    for d in cfg.devices[..cfg.nr_devices as usize].iter() {
        let idx = d.guest_index as usize;
        if devs.len() <= idx {
            devs.resize_with(idx + 1, || None);
        }
        let dev = match d.kind {
            host::VDEV_BLK => Dev::Blk(Device::new(
                d.page_va as usize,
                d.event,
                blk::Blk::new(
                    d.chan_va as usize,
                    cap::<cap_type::Notification>(d.chan_kick_slot),
                ),
            )),
            host::VDEV_NET => Dev::Net(Device::new(
                d.page_va as usize,
                d.event,
                net::Net::new(
                    d.chan_va as usize,
                    cap::<cap_type::Notification>(d.chan_kick_slot),
                    d.mac[..6].try_into().unwrap(),
                ),
            )),
            k => panic!("l4h-vmm: unsupported device kind {}", k),
        };
        devs[idx] = Some(dev);
        by_slot.push(idx);
    }

    let mut console = Console {
        ring: host::VMM_CONSOLE_RING_VA as *mut host::ConsoleRing,
        serial: cfg.serial != 0,
        prefix: if cfg.dom0 != 0 { None } else { Some(cfg.guest_id) },
        line_start: true,
    };
    let root_ep = cap::<cap_type::Endpoint>(host::SLOT_ROOT_EP);
    let ep = cap::<cap_type::Endpoint>(host::SLOT_SERVICE_EP);
    let reply = cap::<cap_type::Reply>(host::SLOT_REPLY);

    set_mr(0, cfg.guest_id as u64);
    root_ep.call(sel4::MessageInfo::new(host::ROOT_CALL_VMM_READY, 0, 0, 1));

    let (mut info, mut badge) = ep.recv(reply);
    loop {
        if badge & host::VMM_BADGE_HYPERCALL == 0 {
            // Notification: device kicks and backend completions.
            for &idx in by_slot.iter() {
                let Some(d) = devs[idx].as_mut() else { continue };
                if badge & (1 << (host::VMM_BADGE_DONE_SHIFT as usize + idx)) != 0 {
                    d.complete(&mem, &vcpus);
                }
                if badge & (1 << (host::VMM_BADGE_KICK_SHIFT as usize + idx)) != 0 {
                    d.poll(&mem, &vcpus);
                }
            }
            (info, badge) = ep.recv(reply);
            continue;
        }
        let resp = match info.label() {
            abi::HC_CONSOLE_WRITE => {
                let len = (mr(0) as usize).min(abi::HC_CONSOLE_MAX_WORDS * 8);
                for i in 0..len {
                    let w = mr(1 + i / 8);
                    console.put((w >> ((i % 8) * 8)) as u8);
                }
                0
            }
            abi::HC_CTL if cfg.dom0 != 0 => {
                set_mr(0, cfg.guest_id as u64);
                root_ep.call(sel4::MessageInfo::new(host::ROOT_CALL_CTL, 0, 0, 1));
                0
            }
            abi::HC_POWER => {
                // l4h-root stops the vCPUs, which cancels this call: do not
                // reply.
                let how = mr(0);
                set_mr(0, cfg.guest_id as u64);
                set_mr(1, how);
                root_ep.call(sel4::MessageInfo::new(host::ROOT_CALL_GUEST_POWER, 0, 0, 2));
                (info, badge) = ep.recv(reply);
                continue;
            }
            abi::HC_DEV_WRITE => {
                let (idx, op, a, b) = (mr(0) as usize, mr(1), mr(2), mr(3));
                match devs.get_mut(idx).and_then(|d| d.as_mut()) {
                    Some(d) => d.op(&mem, &vcpus, op, a, b),
                    None => u64::MAX,
                }
            }
            abi::HC_GET_TIME => 0,
            _ => u64::MAX,
        };
        set_mr(0, resp);
        (info, badge) = ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, 1), reply);
    }
}

l4h_rt::component!(main);
