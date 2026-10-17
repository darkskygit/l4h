//! l4h-root: the l4h root task.  Owns all memory, builds guests, and (for
//! now) serves their hypercalls.  See DESIGN.md §9.

#![no_std]
#![no_main]

extern crate alloc;

mod mem;
mod elf;
mod guest;
mod vspace;
mod window;

use ::alloc::vec::Vec;

use sel4::init_thread::slot;
use sel4::{ObjectBlueprint, cap, cap_type};
use sel4_root_task::{Never, root_task};

use l4h_abi as abi;

use crate::mem::{Alloc, Slots, UtPool};
use crate::guest::{Guest, GuestConfig, HYP_BADGE_FAULT, hyp_badge_guest, hyp_badge_vcpu};

pub const RETYPE_FAN_OUT: usize = 16384;

static DOM0_KERNEL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dom0-kernel"));
static DOM0_INITRD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dom0-initrd"));
static DOM0_CMDLINE: &str = include_str!(concat!(env!("OUT_DIR"), "/dom0-cmdline"));

const fn parse_usize(s: Option<&str>, default: usize) -> usize {
    match s {
        None => default,
        Some(s) => {
            let b = s.as_bytes();
            let mut i = 0;
            let mut v = 0;
            while i < b.len() {
                v = v * 10 + (b[i] - b'0') as usize;
                i += 1;
            }
            v
        }
    }
}
const DOM0_MEM_MB: usize = parse_usize(option_env!("L4H_DOM0_MEM_MB"), 256);
const DOM0_VCPUS: usize = parse_usize(option_env!("L4H_DOM0_VCPUS"), 1);

/// Facts about the host the guests run on.
pub struct HostInfo {
    pub cores: usize,
    pub tsc_hz: u64,
    pub hyp_ep: cap::Endpoint,
    sched_control_base: usize,
    pub timeslice_us: u64,
}

impl HostInfo {
    pub fn sched_control(&self, core: usize) -> cap::SchedControl {
        sel4::init_thread::Slot::<cap_type::SchedControl>::from_index(self.sched_control_base + core).cap()
    }
}

/// ASID pools; x86_64 seL4 has 8 pools of 512 ASIDs each in total.
pub struct AsidPools {
    pools: Vec<(cap::AsidPool, usize)>,
}

const ASIDS_PER_POOL: usize = 1 << sel4::sys::seL4_ASIDPoolIndexBits;

impl AsidPools {
    pub fn assign(&mut self, alloc: &mut Alloc, vspace: cap::VSpace) {
        if !self.pools.iter().any(|(_, used)| *used < ASIDS_PER_POOL) {
            let ut_slot = alloc.slots.alloc();
            let ut = alloc.pool.split(sel4::sys::seL4_ASIDPoolBits as usize, ut_slot).expect("ASID pool memory");
            let pool_slot = alloc.slots.alloc();
            slot::ASID_CONTROL
                .cap()
                .asid_control_make_pool(ut.cap, &slot::CNODE.cap().absolute_cptr(sel4::init_thread::Slot::<cap_type::Null>::from_index(pool_slot).cap()))
                .expect("out of ASID pools");
            self.pools.push((sel4::init_thread::Slot::<cap_type::AsidPool>::from_index(pool_slot).cap(), 0));
        }
        let (pool, used) = self.pools.iter_mut().find(|(_, used)| *used < ASIDS_PER_POOL).unwrap();
        pool.asid_pool_assign(vspace).expect("ASID assign");
        *used += 1;
    }
}

fn tsc_hz(bootinfo: &sel4::BootInfoPtr) -> u64 {
    for extra in bootinfo.extra() {
        if extra.id == sel4::BootInfoExtraId::X86TscFreq {
            let c = extra.content();
            let mhz = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            return mhz as u64 * 1_000_000;
        }
    }
    panic!("no TSC frequency in bootinfo");
}

#[root_task(heap_size = 32 * 1024 * 1024)]
fn main(bootinfo: &sel4::BootInfoPtr) -> sel4::Result<Never> {
    let cores = bootinfo.inner().numNodes as usize;
    let mut alloc = Alloc {
        slots: Slots::new(bootinfo.empty().range()),
        pool: UtPool::from_bootinfo(bootinfo),
    };
    sel4::debug_println!(
        "l4h-root: {} core(s), {} MiB free memory",
        cores,
        alloc.pool.free_bytes() >> 20
    );

    let hyp_ep = alloc.object_cap::<cap_type::Endpoint>(ObjectBlueprint::Endpoint);
    let reply = alloc.object_cap::<cap_type::Reply>(ObjectBlueprint::Reply);
    let host = HostInfo {
        cores,
        tsc_hz: tsc_hz(bootinfo),
        hyp_ep,
        sched_control_base: bootinfo.sched_control().range().start,
        timeslice_us: 5000,
    };
    let mut asids = AsidPools { pools: ::alloc::vec![(slot::ASID_POOL.cap(), 1)] };
    let mut win = window::Window::new(&mut alloc);

    let mut guests: Vec<Guest> = Vec::new();
    if DOM0_KERNEL.is_empty() {
        sel4::debug_println!("l4h-root: no dom0 kernel embedded, nothing to do");
    } else {
        let cfg = GuestConfig {
            name: "dom0",
            ram: DOM0_MEM_MB << 20,
            vcpus: DOM0_VCPUS,
            untyped_quota: 64 << 20,
            vspaces: 64,
            kernel: DOM0_KERNEL,
            initrd: DOM0_INITRD,
            cmdline: DOM0_CMDLINE.trim(),
            dom0: true,
        };
        guests.push(guest::build(0, &cfg, &mut alloc, &mut win, &mut asids, &host));
    }

    serve(&host, &mut alloc, reply, &mut guests)
}

fn mr(i: usize) -> u64 {
    sel4::with_ipc_buffer(|b| b.msg_regs()[i])
}

fn set_mr(i: usize, v: u64) {
    sel4::with_ipc_buffer_mut(|b| b.msg_regs_mut()[i] = v)
}

/// Serve hypercalls and guest-kernel faults.
fn serve(host: &HostInfo, alloc: &mut Alloc, mut reply: cap::Reply, guests: &mut [Guest]) -> ! {
    // A reply object that we leave unanswered (a faulted or stopped vCPU)
    // stays bound to its caller; continue with a fresh one.
    let mut abandon = |reply: &mut cap::Reply| {
        *reply = alloc.object_cap::<cap_type::Reply>(ObjectBlueprint::Reply);
    };
    let (mut info, mut badge) = host.hyp_ep.recv(reply);
    loop {
        let gid = hyp_badge_guest(badge);
        let vcpu = hyp_badge_vcpu(badge);
        if badge & HYP_BADGE_FAULT != 0 {
            let fault = sel4::with_ipc_buffer(|b| sel4::Fault::new(b, &info));
            sel4::debug_println!("l4h-root: guest {} vCPU {} kernel fault: {:x?}", gid, vcpu, fault);
            if let Some(g) = guests.get_mut(gid) {
                g.running = false;
            }
            abandon(&mut reply);
            (info, badge) = host.hyp_ep.recv(reply);
            continue;
        }
        let resp = match info.label() {
            abi::HC_CONSOLE_WRITE => {
                let len = (mr(0) as usize).min(abi::HC_CONSOLE_MAX_WORDS * 8);
                for i in 0..len {
                    let w = mr(1 + i / 8);
                    sel4::debug_put_char((w >> ((i % 8) * 8)) as u8);
                }
                0
            }
            abi::HC_POWER => {
                sel4::debug_println!("l4h-root: guest {} power request {}", gid, mr(0));
                if let Some(g) = guests.get_mut(gid) {
                    for v in &g.vcpus {
                        let _ = v.ktcb.tcb_suspend();
                    }
                    g.running = false;
                }
                if guests.iter().all(|g| !g.running) {
                    sel4::debug_println!("l4h-root: all guests stopped");
                }
                abandon(&mut reply);
                (info, badge) = host.hyp_ep.recv(reply);
                continue;
            }
            abi::HC_GET_TIME => 0,
            other => {
                sel4::debug_println!("l4h-root: guest {} unknown hypercall {}", gid, other);
                u64::MAX
            }
        };
        set_mr(0, resp);
        (info, badge) = host.hyp_ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, 1), reply);
    }
}
