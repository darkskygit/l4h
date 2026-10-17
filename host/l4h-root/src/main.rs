//! l4h-root: the l4h root task.  Owns all memory and devices, creates and
//! destroys guests with their host components, and serves the control API
//! for dom0.  See DESIGN.md §9.

#![no_std]
#![no_main]

extern crate alloc;

mod blkd;
mod component;
mod ctl;
mod devmem;
mod elf;
mod guest;
mod mem;
mod netd;
mod pci;
mod sched;
mod vmm;
mod vspace;
mod window;

use ::alloc::vec::Vec;

use sel4::init_thread::slot;
use sel4::{ObjectBlueprint, cap, cap_type};
use sel4_root_task::{Never, root_task};

use l4h_host as host;

use crate::blkd::Disk;
use crate::component::ROOT_BADGE_COMPONENT;
use crate::guest::{Guest, GuestConfig, HYP_BADGE_FAULT, State, hyp_badge_guest, hyp_badge_vcpu};
use crate::mem::{Alloc, Slots};
use crate::window::{Window, Windows};

pub const RETYPE_FAN_OUT: usize = 16384;
pub const MAX_GUESTS: usize = 64;

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
/* Disks passed to dom0 at boot (the first n); the rest are free for the
 * control API. */
const DOM0_DISKS: usize = parse_usize(option_env!("L4H_DOM0_DISKS"), 64);
/* vNICs of dom0 on switch 0, which has the first NIC as its uplink. */
const DOM0_NICS: usize = parse_usize(option_env!("L4H_DOM0_NICS"), 1);
/* Seconds between watchdog dumps of guest vCPU state; 0 disables. */
const WATCHDOG_SECS: usize = parse_usize(option_env!("L4H_WATCHDOG_SECS"), 0);
const TIMER_BADGE: u64 = 1 << 40;

/// Facts about the host the guests run on.
pub struct HostInfo {
    pub cores: usize,
    pub tsc_hz: u64,
    pub hyp_ep: cap::Endpoint,
    sched_control_base: usize,
    pub timeslice_us: u64,
    /// Round-robin placement of new guests.
    placement: core::cell::Cell<usize>,
}

impl HostInfo {
    /// Core for the next guest's vCPU 0.
    pub fn next_core(&self) -> usize {
        let c = self.placement.get();
        self.placement.set(c + 1);
        c % self.cores
    }

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
        for (pool, used) in self.pools.iter_mut() {
            if *used < ASIDS_PER_POOL {
                if pool.asid_pool_assign(vspace).is_ok() {
                    *used += 1;
                    return;
                }
                *used = ASIDS_PER_POOL;
            }
        }
        // ASIDs of deleted VSpaces are freed by the kernel, so "full" pools
        // may have room again.
        for (pool, used) in self.pools.iter_mut() {
            if pool.asid_pool_assign(vspace).is_ok() {
                *used = ASIDS_PER_POOL / 2;
                return;
            }
        }
        let (ut_slot, pool_slot) = alloc.slots.global(|slots| (slots.alloc(), slots.alloc()));
        let ut = alloc.split(sel4::sys::seL4_ASIDPoolBits as usize, ut_slot).expect("ASID pool memory");
        slot::ASID_CONTROL
            .cap()
            .asid_control_make_pool(ut.cap, &slot::CNODE.cap().absolute_cptr(sel4::init_thread::Slot::<cap_type::Null>::from_index(pool_slot).cap()))
            .expect("out of ASID pools");
        let pool = sel4::init_thread::Slot::<cap_type::AsidPool>::from_index(pool_slot).cap();
        pool.asid_pool_assign(vspace).expect("ASID assign");
        self.pools.push((pool, 1));
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

/// All state of the root task.
pub struct Host {
    pub info: HostInfo,
    pub alloc: Alloc,
    pub wins: Windows,
    pub asids: AsidPools,
    pub pci: pci::Pci,
    pub devmem: devmem::DevMem,
    pub disks: Vec<Disk>,
    pub netd: netd::Netd,
    pub guests: Vec<Option<Guest>>,
    pub total_bytes: usize,
}

impl Host {
    /// Run `f` on guest `id` with root slot allocations tracked into it.
    pub fn with_guest<R>(&mut self, id: usize, f: impl FnOnce(&mut Host, &mut Guest) -> R) -> Option<R> {
        let mut g = self.guests.get_mut(id)?.take()?;
        self.alloc.slots.track(core::mem::take(&mut g.slots));
        let r = f(self, &mut g);
        g.slots = self.alloc.slots.untrack();
        self.guests[id] = Some(g);
        Some(r)
    }

    pub fn guest(&self, id: usize) -> Option<&Guest> {
        self.guests.get(id)?.as_ref()
    }

    pub fn create_guest(&mut self, cfg: &GuestConfig) -> Option<usize> {
        let id = (0..MAX_GUESTS).find(|&i| self.guest(i).is_none())?;
        if self.guests.len() <= id {
            self.guests.resize_with(id + 1, || None);
        }
        self.alloc.slots.track(Default::default());
        let g = guest::create(id, cfg, &mut self.alloc, &mut self.asids, &self.info);
        let slots = self.alloc.slots.untrack();
        let mut g = g?;
        g.slots = slots;
        self.guests[id] = Some(g);
        Some(id)
    }

    /// Pass host disk `disk` (or sectors [lba_start, lba_start + lba_count)
    /// of it; 0 = to the end) through to guest `id` before it starts.
    pub fn attach_disk(&mut self, id: usize, disk: usize, read_only: bool, lba_start: u64, lba_count: u64) -> Result<(), i64> {
        if self.disks.get(disk).ok_or(ctl::ENOENT)?.owner.is_some() {
            return Err(ctl::EBUSY);
        }
        self.with_guest(id, |h, g| {
            if g.state != State::Created {
                return Err(ctl::EBUSY);
            }
            let d = g.add_device(&mut h.alloc, guest::DevKind::Virtio(2)).ok_or(ctl::ENOSPC)?;
            let backend = blkd::attach(
                0x100 + disk as u32,
                &mut h.disks[disk],
                &h.pci,
                g,
                d,
                (read_only, lba_start, lba_count),
                &mut h.devmem,
                &mut h.alloc,
                &mut h.wins.a,
                &mut h.asids,
                &h.info,
            );
            h.disks[disk].read_only = read_only;
            g.backends.push((d, backend));
            g.disks.push(disk);
            Ok(())
        })
        .ok_or(ctl::ENOENT)?
    }

    /// Spawn the guest's l4h-vmm; the guest starts when the VMM is ready.
    pub fn start_guest(&mut self, id: usize) -> Result<(), i64> {
        self.with_guest(id, |h, g| {
            if g.state != State::Created || g.vmm.is_some() {
                return Err(ctl::EBUSY);
            }
            let v = vmm::spawn(g, &mut h.alloc, &mut h.wins.a, &mut h.asids, &h.info);
            component::start(&v);
            g.vmm = Some(v);
            Ok(())
        })
        .ok_or(ctl::ENOENT)?
    }

    /// Give guest `id` (before it starts) a virtio-net device on `switch`.
    /// Returns the MAC.
    pub fn create_nic(&mut self, id: usize, switch: u32, mac: u64, lock: bool) -> Result<u64, i64> {
        self.with_guest(id, |h, g| {
            if g.state != State::Created || g.vmm.is_some() {
                return Err(ctl::EBUSY);
            }
            let d = g.add_device(&mut h.alloc, guest::DevKind::Virtio(1)).ok_or(ctl::ENOSPC)?;
            let next = h.netd.ports.iter().position(|p| p.is_none()).unwrap_or(0) as u64;
            let mac = if mac == 0 { 0x02_4c_34_00_00_00 | ((id as u64) << 8) | next } else { mac & 0xffff_ffff_ffff };
            if mac >> 40 & 1 != 0 {
                return Err(ctl::EINVAL);
            }
            let (p, backend) = h.netd.add_port(g, d, switch, mac, lock, &mut h.alloc)?;
            g.backends.push((d, backend));
            g.ports.push(p);
            Ok(mac)
        })
        .ok_or(ctl::ENOENT)?
    }

    pub fn destroy_guest(&mut self, id: usize) -> Result<(), i64> {
        let mut g = self.guests.get_mut(id).and_then(|g| g.take()).ok_or(ctl::ENOENT)?;
        g.stop();
        for &p in &g.ports {
            self.netd.remove_port(p);
        }
        for &d in &g.disks {
            blkd::detach(&mut self.disks[d], &self.pci, &mut self.alloc);
        }
        let name = g.name.clone();
        g.destroy(&mut self.alloc);
        sel4::debug_println!(
            "l4h-root: guest {} '{}' destroyed, {} MiB free",
            id, name, self.alloc.free_bytes() >> 20
        );
        Ok(())
    }
}

#[root_task(heap_size = 32 * 1024 * 1024)]
fn main(bootinfo: &sel4::BootInfoPtr) -> sel4::Result<Never> {
    let cores = bootinfo.inner().numNodes as usize;
    let mut alloc = Alloc::new(Slots::new(bootinfo.empty().range()), bootinfo);
    let total_bytes = alloc.free_bytes();
    sel4::debug_println!("l4h-root: {} core(s), {} MiB free memory", cores, total_bytes >> 20);

    let hyp_ep = alloc.object_cap::<cap_type::Endpoint>(ObjectBlueprint::Endpoint);
    // The watchdog timer signals a notification bound to the root TCB, which
    // wakes the serve loop out of its receive on hyp_ep.
    let wd_ntfn = alloc.object_cap::<cap_type::Notification>(ObjectBlueprint::Notification);
    slot::TCB.cap().tcb_bind_notification(wd_ntfn).expect("bind watchdog notification");
    let reply = alloc.object_cap::<cap_type::Reply>(ObjectBlueprint::Reply);
    let info = HostInfo {
        cores,
        tsc_hz: tsc_hz(bootinfo),
        hyp_ep,
        sched_control_base: bootinfo.sched_control().range().start,
        timeslice_us: 5000,
        placement: core::cell::Cell::new(0),
    };
    let asids = AsidPools { pools: ::alloc::vec![(slot::ASID_POOL.cap(), 1)] };
    let wins = Windows { a: Window::new(&mut alloc, 0), b: Window::new(&mut alloc, 1) };

    // PCI devices: virtio-blk disks can be passed through to guests.
    let pci = pci::Pci::new(&mut alloc);
    let mut disks = Vec::new();
    let mut nics = Vec::new();
    for d in pci.enumerate() {
        sel4::debug_println!(
            "l4h-root: pci {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}",
            d.bus, d.dev, d.func, d.vendor, d.device, d.class, d.subclass
        );
        if blkd::is_disk(&d) {
            // Stop whatever the firmware left running.
            pci.disable(&d);
            let v = disks.len() as u64;
            disks.push(Disk::new(d, v));
        } else if netd::is_virtio_net(&d) {
            pci.disable(&d);
            nics.push(d);
        }
    }
    let mut devmem = devmem::DevMem::from_bootinfo(bootinfo);
    let mut wins = wins;
    let mut asids = asids;
    let netd = netd::spawn(nics, &pci, &mut devmem, &mut alloc, &mut wins.a, &mut asids, &info);

    let mut h = Host {
        info,
        alloc,
        wins,
        asids,
        pci,
        devmem,
        disks,
        netd,
        guests: Vec::new(),
        total_bytes,
    };

    if DOM0_KERNEL.is_empty() {
        sel4::debug_println!("l4h-root: no dom0 kernel embedded, nothing to do");
    } else {
        boot_dom0(&mut h);
    }
    serve(&mut h, reply)
}

fn boot_dom0(h: &mut Host) {
    let ram_base = elf::Elf::parse(DOM0_KERNEL).expect("dom0 kernel").bounds().0 as usize;
    let cfg = GuestConfig {
        name: "dom0",
        ram: DOM0_MEM_MB << 20,
        vcpus: DOM0_VCPUS,
        untyped_quota: 64 << 20,
        vspaces: 64,
        ram_base,
        dom0: true,
    };
    let id = h.create_guest(&cfg).expect("cannot create dom0");
    for d in 0..h.disks.len().min(DOM0_DISKS) {
        h.attach_disk(id, d, false, 0, 0).expect("attach disk to dom0");
    }
    // Switch 0, with the first NIC (if any) as its uplink (on first boot).
    let sw = 0;
    if h.netd.switches == 0 {
        h.netd.create_switch();
        if !h.netd.nics.is_empty() {
            h.netd.attach_uplink(sw, 0).expect("uplink");
        }
    }
    for _ in 0..DOM0_NICS {
        h.create_nic(id, sw, 0, true).expect("dom0 NIC");
    }
    h.with_guest(id, |h, g| {
        assert!(
            guest::load_embedded(g, &mut h.wins.a, &h.info, DOM0_KERNEL, DOM0_INITRD, DOM0_CMDLINE.trim()),
            "dom0 image does not fit"
        );
    });
    h.start_guest(id).expect("start dom0");
}

/// dom0 rebooted or crashed: replace it.  Other guests keep running; the
/// new dom0 finds them with the control API.
fn restart_dom0(h: &mut Host, id: usize) {
    sel4::debug_println!("l4h-root: restarting dom0");
    let _ = h.destroy_guest(id);
    boot_dom0(h);
}

fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi) };
    ((hi as u64) << 32) | lo as u64
}

/// The root task's periodic timer: balancer and watchdog.
fn arm_timer(info: &HostInfo) {
    let deadline = rdtsc() + info.tsc_hz * sched::BALANCE_MS / 1000;
    let tcb = slot::TCB.cap();
    sel4::with_ipc_buffer_mut(|ipc| ipc.inner_mut().seL4_TCB_SetTimer(tcb.bits(), deadline, TIMER_BADGE));
}

fn watchdog_dump(h: &Host) {
    for g in h.guests.iter().flatten() {
        if g.state != State::Running {
            continue;
        }
        for (v, vcpu) in g.vcpus.iter().enumerate() {
            match vcpu.ktcb.tcb_read_all_registers(false) {
                Ok(r) => sel4::debug_println!(
                    "l4h-root: watchdog: guest {} vCPU {} rip={:#x} rsp={:#x}",
                    g.id, v, *r.pc(), *r.sp()
                ),
                Err(e) => sel4::debug_println!("l4h-root: watchdog: guest {} vCPU {}: {:?}", g.id, v, e),
            }
        }
    }
}

pub fn mr(i: usize) -> u64 {
    sel4::with_ipc_buffer(|b| b.msg_regs()[i])
}

pub fn set_mr(i: usize, v: u64) {
    sel4::with_ipc_buffer_mut(|b| b.msg_regs_mut()[i] = v)
}

/// Serve calls from host components and faults of guest kernels.
fn serve(h: &mut Host, mut reply: cap::Reply) -> ! {
    let hyp_ep = h.info.hyp_ep;
    arm_timer(&h.info);
    let mut next_watchdog = rdtsc() + h.info.tsc_hz * WATCHDOG_SECS as u64;
    let mut last_balance = rdtsc();
    let (mut info, mut badge) = hyp_ep.recv(reply);
    loop {
        h.alloc.refill();
        let resp = if badge & TIMER_BADGE != 0 {
            let now = rdtsc();
            sched::balance(h, (now - last_balance) * 1_000_000 / h.info.tsc_hz);
            last_balance = now;
            if WATCHDOG_SECS != 0 && rdtsc() >= next_watchdog {
                watchdog_dump(h);
                next_watchdog = rdtsc() + h.info.tsc_hz * WATCHDOG_SECS as u64;
            }
            arm_timer(&h.info);
            None
        } else if badge & ROOT_BADGE_COMPONENT != 0 {
            component_call(h, badge, &info)
        } else if badge & HYP_BADGE_FAULT != 0 {
            let gid = hyp_badge_guest(badge);
            let vcpu = hyp_badge_vcpu(badge);
            let fault = sel4::with_ipc_buffer(|b| sel4::Fault::new(b, &info));
            sel4::debug_println!("l4h-root: guest {} vCPU {} kernel fault: {:x?}", gid, vcpu, fault);
            if h.guest(gid).is_some_and(|g| g.dom0) {
                restart_dom0(h, gid);
            } else if let Some(Some(g)) = h.guests.get_mut(gid) {
                g.stop();
            }
            None
        } else {
            sel4::debug_println!("l4h-root: unexpected message, badge {:#x} label {}", badge, info.label());
            None
        };
        match resp {
            Some(v) => {
                set_mr(0, v);
                (info, badge) = hyp_ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, 1), reply);
            }
            None => {
                // A reply object left unanswered (a faulted or stopped
                // thread) stays bound to its caller: use a fresh one.
                if badge & TIMER_BADGE == 0 {
                    reply = h.alloc.object_cap::<cap_type::Reply>(ObjectBlueprint::Reply);
                }
                (info, badge) = hyp_ep.recv(reply);
            }
        }
    }
}

/// A call or fault from a host component.  Returns the reply value, or
/// None to leave the caller blocked.
fn component_call(h: &mut Host, badge: u64, info: &sel4::MessageInfo) -> Option<u64> {
    let id = (badge & 0xffff) as usize;
    if badge & HYP_BADGE_FAULT != 0 {
        let fault = sel4::with_ipc_buffer(|b| sel4::Fault::new(b, info));
        sel4::debug_println!("l4h-root: component {:#x} fault: {:x?}", id, fault);
        return None;
    }
    // Component ids below 0x100 are guest VMMs; the id is the guest's.
    let is_vmm = id < MAX_GUESTS;
    match info.label() {
        host::ROOT_CALL_VMM_READY if is_vmm => {
            h.guests.get_mut(id)?.as_mut()?.start();
            Some(0)
        }
        host::ROOT_CALL_GUEST_POWER if is_vmm => {
            let how = mr(1);
            sel4::debug_println!("l4h-root: guest {} power request {}", id, how);
            if how != l4h_abi::POWER_OFF && h.guest(id).is_some_and(|g| g.dom0) {
                // The caller (dom0's VMM) is destroyed: no reply.
                restart_dom0(h, id);
                return None;
            }
            if let Some(Some(g)) = h.guests.get_mut(id) {
                for v in &g.vcpus {
                    let _ = v.ktcb.tcb_suspend();
                }
                g.state = State::Stopped;
            }
            if h.guests.iter().flatten().all(|g| g.state != State::Running) {
                sel4::debug_println!("l4h-root: all guests stopped");
            }
            Some(0)
        }
        host::ROOT_CALL_CTL if is_vmm && h.guest(id).is_some_and(|g| g.dom0) => {
            ctl::handle(h, id);
            Some(0)
        }
        other => {
            sel4::debug_println!("l4h-root: component {:#x}: unknown call {}", id, other);
            Some(u64::MAX)
        }
    }
}
