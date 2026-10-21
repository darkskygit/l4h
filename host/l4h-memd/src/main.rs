//! l4h-memd: the owner of the host's free memory and of every guest's RAM
//! (DESIGN.md §16).
//!
//! At boot, l4h-root moves every free RAM Untyped here.  From then on it
//! asks for memory for its own objects (MEMD_ALLOC) and returns it when it
//! is free again (MEMD_FREE), and guest RAM is taken, given and revoked
//! here: memd keeps the original Untyped of every 2 MiB block of a guest,
//! hands copies to l4h-root (for the boot blocks it fills with frames) and
//! to the guest, and revokes the originals when the guest is destroyed,
//! which deletes everything made of them.  memd also reports free memory
//! to the hypervisor l4h runs in (balloon.rs).
//!
//! memd serves l4h-root only, one call at a time, and never calls it.  It
//! cannot read memory: every Untyped it has handed out has a child, so it
//! cannot retype it, and revoking it first makes seL4 clear the memory
//! before it is used again.

#![no_std]
#![no_main]

extern crate alloc;

mod balloon;
mod mem;

use alloc::vec::Vec;

use l4h_rt::host::memd::{self, BalloonConfig, MemdConfig, Status};
use l4h_rt::{Info, cap, host, sel4};
use sel4::{CapRights, cap_type};

use crate::balloon::Balloon;
use crate::mem::{Mem, Slot, Slots, abs};

const ENODEV: u64 = 19;
const ENOMEM: u64 = 12;
const EINVAL: u64 = 22;

/// How often the free page reports are made and polled.
const TICK_MS: u64 = 100;

/// A guest's RAM: the original Untyped of each block (slot, paddr).
struct GuestRam {
    blocks: Vec<(Slot, usize)>,
    /// Radix of its block CNode.
    radix: usize,
}

struct Memd {
    mem: Mem,
    guests: Vec<Option<GuestRam>>,
    ticks_per_ms: u64,
}

fn mr(i: usize) -> u64 {
    sel4::with_ipc_buffer(|b| b.msg_regs()[i])
}

fn set_mr(i: usize, v: u64) {
    sel4::with_ipc_buffer_mut(|b| b.msg_regs_mut()[i] = v)
}

fn xchg(i: usize) -> sel4::AbsoluteCPtr {
    abs(memd::SLOT_XCHG + i as u64)
}

impl Memd {
    /// MEMD_ALLOC: `count` blocks of 2^bits bytes into the exchange slots.
    fn alloc(&mut self, bits: usize, count: usize) -> u64 {
        if count > memd::XCHG_SLOTS || !(12..=47).contains(&bits) {
            return EINVAL;
        }
        let mut got = Vec::new();
        for _ in 0..count {
            match self.mem.take(bits) {
                Some(b) => got.push(b),
                None => {
                    for (s, paddr) in got {
                        self.mem.add(s, paddr, bits);
                    }
                    return ENOMEM;
                }
            }
        }
        for (i, &(s, _)) in got.iter().enumerate() {
            xchg(i).move_(&abs(s)).expect("memd: move to exchange");
            self.mem.slots.free(s);
        }
        // Invocations use the message registers: the reply's go last.
        for (i, (_, paddr)) in got.into_iter().enumerate() {
            set_mr(1 + i, paddr as u64);
        }
        0
    }

    /// MEMD_FREE: blocks from the exchange slots.
    fn free(&mut self, count: usize) -> u64 {
        if count > memd::XCHG_SLOTS {
            return EINVAL;
        }
        // Read them all first: invocations use the message registers.
        let blocks: Vec<(usize, usize)> = (0..count).map(|i| (mr(1 + 2 * i) as usize, mr(2 + 2 * i) as usize)).collect();
        for (i, (paddr, bits)) in blocks.into_iter().enumerate() {
            let s = self.mem.slots.alloc();
            abs(s).move_(&xchg(i)).expect("memd: move from exchange");
            self.mem.add(s, paddr, bits);
        }
        0
    }

    fn guest(&self, g: usize) -> Option<&GuestRam> {
        self.guests.get(g)?.as_ref()
    }

    fn guest_create(&mut self, g: usize, n: usize, radix: usize) -> u64 {
        if g >= memd::MAX_GUESTS || self.guests[g].is_some() || n == 0 || radix > 20 || n > 1 << radix {
            return EINVAL;
        }
        let mut blocks = Vec::with_capacity(n);
        while blocks.len() < n {
            match self.mem.take(memd::BLOCK_BITS) {
                Some(b) => blocks.push(b),
                None => {
                    for (s, paddr) in blocks {
                        self.mem.add(s, paddr, memd::BLOCK_BITS);
                    }
                    return ENOMEM;
                }
            }
        }
        self.guests[g] = Some(GuestRam { blocks, radix });
        0
    }

    fn guest_copy(&mut self, g: usize, b: usize) -> u64 {
        let Some(&(s, paddr)) = self.guest(g).and_then(|r| r.blocks.get(b)) else {
            return EINVAL;
        };
        // Fails if the block already has a copy.
        if xchg(0).copy(&abs(s), CapRights::all()).is_err() {
            return EINVAL;
        }
        set_mr(1, paddr as u64);
        0
    }

    fn guest_give(&mut self, g: usize, first: usize) -> u64 {
        let Some(r) = self.guest(g) else { return EINVAL };
        let bcn = cap::<cap_type::CNode>(memd::SLOT_GUEST_BCN + g as u64);
        for (b, &(s, _)) in r.blocks.iter().enumerate().skip(first) {
            // A block without children: its copy is a sibling Untyped
            // that the guest can retype and revoke, and revoking the
            // original still deletes everything made from either.
            if bcn.absolute_cptr_from_bits_with_depth(b as u64, r.radix).copy(&abs(s), CapRights::all()).is_err() {
                return EINVAL;
            }
        }
        0
    }

    fn guest_destroy(&mut self, g: usize) -> u64 {
        if g >= memd::MAX_GUESTS {
            return EINVAL;
        }
        if let Some(r) = self.guests[g].take() {
            for (s, paddr) in r.blocks {
                abs(s).revoke().expect("memd: revoke guest block");
                self.mem.add(s, paddr, memd::BLOCK_BITS);
            }
        }
        let _ = abs(memd::SLOT_GUEST_BCN + g as u64).delete();
        0
    }

    fn balloon(&mut self) -> u64 {
        let cfg = BalloonConfig::from_mrs(core::array::from_fn(mr));
        if self.mem.balloon.is_some() {
            return EINVAL;
        }
        self.mem.balloon = Balloon::new(&cfg, self.ticks_per_ms);
        if self.mem.balloon.is_none() {
            return ENODEV;
        }
        arm_timer(self.ticks_per_ms);
        0
    }

    fn write_status(&self) {
        let (blocks, largest) = self.mem.block_stats();
        let guest_blocks: usize = self.guests.iter().flatten().map(|r| r.blocks.len()).sum();
        let st = Status {
            free_bytes: self.mem.free_bytes() as u64,
            reported_bytes: self.mem.reported_bytes() as u64,
            free_blocks: blocks as u64,
            largest_block: largest as u64,
            guest_bytes: (guest_blocks << memd::BLOCK_BITS) as u64,
        };
        unsafe { (memd::STATUS_VA as *mut Status).write_volatile(st) };
    }
}

fn arm_timer(ticks_per_ms: u64) {
    let tcb = cap::<cap_type::Tcb>(host::SLOT_TCB);
    let deadline = host::ticks::now() + TICK_MS * ticks_per_ms;
    sel4::with_ipc_buffer_mut(|ipc| ipc.inner_mut().seL4_TCB_SetTimer(tcb.bits(), deadline, memd::BADGE_TIMER));
}

fn main(info: &'static Info) -> ! {
    let cfg: &MemdConfig = unsafe { info.config() };
    let mut d = Memd {
        mem: Mem::new(Slots::new(memd::SLOT_DYN_BASE..1 << cfg.cnode_bits)),
        guests: (0..memd::MAX_GUESTS).map(|_| None).collect(),
        ticks_per_ms: info.header().tsc_hz / 1000,
    };
    d.write_status();
    let ep = cap::<cap_type::Endpoint>(host::SLOT_SERVICE_EP);
    let reply = cap::<cap_type::Reply>(host::SLOT_REPLY);

    let (mut msg, mut badge) = ep.recv(reply);
    loop {
        if badge != 0 {
            // The timer.
            let delay = balloon::REPORT_DELAY_MS * d.ticks_per_ms;
            d.mem.report_free(host::ticks::now(), delay);
            d.write_status();
            arm_timer(d.ticks_per_ms);
            (msg, badge) = ep.recv(reply);
            continue;
        }
        // A call from l4h-root.
        let mut len = 1;
        let resp = match msg.label() {
            memd::MEMD_ALLOC => {
                let count = mr(1) as usize;
                len += count;
                d.alloc(mr(0) as usize, count)
            }
            memd::MEMD_FREE => d.free(mr(0) as usize),
            memd::MEMD_GUEST_CREATE => d.guest_create(mr(0) as usize, mr(1) as usize, mr(2) as usize),
            memd::MEMD_GUEST_COPY => {
                len = 2;
                d.guest_copy(mr(0) as usize, mr(1) as usize)
            }
            memd::MEMD_GUEST_GIVE => d.guest_give(mr(0) as usize, mr(1) as usize),
            memd::MEMD_GUEST_DESTROY => d.guest_destroy(mr(0) as usize),
            memd::MEMD_BALLOON => d.balloon(),
            _ => EINVAL,
        };
        d.write_status();
        set_mr(0, resp);
        (msg, badge) = ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, len.min(memd::XCHG_SLOTS + 1)), reply);
    }
}

// The free blocks and the guests' block tables: about 50 bytes per 2 MiB
// of host memory.
l4h_rt::component!(main, heap_size = 8 * 1024 * 1024);
