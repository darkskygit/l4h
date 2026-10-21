//! l4h-root: the l4h root task.  Owns all memory and devices, creates and
//! destroys guests with their host components, and serves the control API
//! for dom0.  See DESIGN.md §9.

#![no_std]
#![no_main]

extern crate alloc;

/// An informational message: printed only when `host::VERBOSE` (not in
/// release builds).  Use `sel4::debug_println!` for critical errors only.
macro_rules! info {
    ($($arg:tt)*) => {
        if l4h_host::VERBOSE {
            sel4::debug_println!($($arg)*);
        }
    };
}

mod balloon;
mod blkd;
mod component;
mod ctl;
mod dashboard;
mod devmem;
mod elf;
mod font;
mod guest;
mod mem;
mod monitor;
mod netd;
mod pci;
mod rtc;
mod sched;
mod vmm;
mod vspace;
mod window;

use ::alloc::vec::Vec;

use sel4::init_thread::slot;
use sel4::{ObjectBlueprint, cap, cap_type};
use sel4_root_task::{Never, root_task};

use l4h_host as host;

use crate::blkd::{Controller, Disk};
use crate::component::ROOT_BADGE_COMPONENT;
use crate::guest::{Guest, GuestConfig, HYP_BADGE_FAULT, State, hyp_badge_guest, hyp_badge_vcpu};
use crate::mem::{Alloc, Slots};
use crate::window::{Window, Windows};

pub const RETYPE_FAN_OUT: usize = 16384;
pub const MAX_GUESTS: usize = 64;
/// Memory and root CNode slots kept back from guest requests.
const RESERVE_BYTES: usize = 32 << 20;
const RESERVE_SLOTS: usize = 32768;
/// Longest free slot range kept for vNIC ports and disk DMA buffers.
const RESERVE_RANGE: usize = 4096;

static DOM0_KERNEL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dom0-kernel"));
static DOM0_INITRD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dom0-initrd"));
static DOM0_CMDLINE: &str = include_str!(concat!(env!("OUT_DIR"), "/dom0-cmdline"));

/// A decimal number, or hexadecimal after "0x".
const fn parse_usize(s: Option<&str>, default: usize) -> usize {
    match s {
        None => default,
        Some(s) => {
            let b = s.as_bytes();
            let (mut i, radix) = if b.len() > 2 && b[0] == b'0' && b[1] == b'x' { (2, 16) } else { (0, 10) };
            let mut v = 0;
            while i < b.len() {
                let d = match b[i] {
                    b'0'..=b'9' => b[i] - b'0',
                    b'a'..=b'f' => b[i] - b'a' + 10,
                    b'A'..=b'F' => b[i] - b'A' + 10,
                    _ => panic!("not a number"),
                };
                v = v * radix + d as usize;
                i += 1;
            }
            v
        }
    }
}

/// A PCI address "bus:device.function" (hexadecimal) as bus << 8 |
/// device << 3 | function, or None.
const fn parse_bdf(s: Option<&str>) -> Option<u32> {
    let Some(s) = s else { return None };
    let b = s.as_bytes();
    let mut v = [0u32; 3];
    let (mut i, mut f) = (0, 0);
    while i < b.len() {
        match b[i] {
            b':' | b'.' => f += 1,
            c => {
                let d = match c {
                    b'0'..=b'9' => c - b'0',
                    b'a'..=b'f' => c - b'a' + 10,
                    b'A'..=b'F' => c - b'A' + 10,
                    _ => panic!("not a PCI address"),
                };
                v[f] = v[f] * 16 + d as u32;
            }
        }
        i += 1;
    }
    Some(v[0] << 8 | v[1] << 3 | v[2])
}
const DOM0_MEM_MB: usize = parse_usize(option_env!("L4H_DOM0_MEM_MB"), 256);
const DOM0_VCPUS: usize = parse_usize(option_env!("L4H_DOM0_VCPUS"), 1);
/* dom0's I/O pool (a power of two, 1-256 MiB). */
const DOM0_IO_POOL_MB: usize = parse_usize(option_env!("L4H_DOM0_IO_POOL_MB"), guest::IO_POOL_DEFAULT >> 20);
/* Disks passed to dom0 at boot (the first n); the rest are free for the
 * control API. */
const DOM0_DISKS: usize = parse_usize(option_env!("L4H_DOM0_DISKS"), 64);
/* vNICs of dom0 on switch 0, which has the first NIC as its uplink. */
const DOM0_NICS: usize = parse_usize(option_env!("L4H_DOM0_NICS"), 1);
/* The PCI address (bus:dev.fn) of the uplink NIC, when it is not the first
 * NIC: e.g. the public port of a two-port card. */
const UPLINK_NIC: Option<u32> = parse_bdf(option_env!("L4H_UPLINK_NIC"));
/* The PCI address of a second uplink NIC: the uplink of switch 1, where
 * dom0 has one more vNIC, with the NIC's MAC address; the internal
 * switches follow. E.g. a provider's storage network on the other port. */
const UPLINK2_NIC: Option<u32> = parse_bdf(option_env!("L4H_UPLINK2_NIC"));
/* The PCI address of the boot disk's controller, when it is not the first
 * disk controller: it becomes host disk 0. */
const BOOT_DISK: Option<u32> = parse_bdf(option_env!("L4H_BOOT_DISK"));
/* Internal switches, after switch 0 and the second uplink's (1..=n, or
 * 2..=n+1), created at first boot, each with one more vNIC of dom0 (a
 * normal port, for routing between it and isolated guests). */
const DOM0_INTERNAL_SWITCHES: usize = parse_usize(option_env!("L4H_DOM0_INTERNAL_SWITCHES"), 0);
/* Seconds between watchdog dumps of guest vCPU state; 0 disables. */
const WATCHDOG_SECS: usize = parse_usize(option_env!("L4H_WATCHDOG_SECS"), 0);
const TIMER_BADGE: u64 = 1 << 40;

/// Facts about the host the guests run on.
pub struct HostInfo {
    pub cores: usize,
    pub tsc_hz: u64,
    /// AArch64 feature ID registers for the guests' bootinfo (`cpu_id`).
    pub cpu_id: [u64; 5],
    pub hyp_ep: cap::Endpoint,
    sched_control_base: usize,
    pub timeslice_us: u64,
    /// Wall clock at boot (ns since the epoch, 0 if unknown) and the tick
    /// count when it was read.
    pub epoch_ns: u64,
    pub epoch_ticks: u64,
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
    /// ASIDs assigned to live VSpaces (exact: guests return theirs).
    pub in_use: usize,
}

const ASID_CAPACITY: usize = (1 << sel4::sys::seL4_NumASIDPoolsBits) * ASIDS_PER_POOL;

const ASIDS_PER_POOL: usize = 1 << sel4::sys::seL4_ASIDPoolIndexBits;

impl AsidPools {
    pub fn available(&self) -> usize {
        ASID_CAPACITY.saturating_sub(self.in_use)
    }

    pub fn assign(&mut self, alloc: &mut Alloc, vspace: cap::VSpace) {
        assert!(self.in_use < ASID_CAPACITY, "out of ASIDs");
        self.in_use += 1;
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

#[cfg(target_arch = "aarch64")]
fn tsc_hz(_bootinfo: &sel4::BootInfoPtr) -> u64 {
    host::ticks::hz()
}

#[cfg(target_arch = "x86_64")]
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

/// ID_AA64ISAR0/ISAR1/ISAR2/PFR0/PFR1_EL1 of the boot CPU, from the
/// bootinfo chunk added by kernel patch 0018; zero if it is missing.
#[cfg(target_arch = "aarch64")]
fn cpu_id(bootinfo: &sel4::BootInfoPtr) -> [u64; 5] {
    // SEL4_BOOTINFO_HEADER_AARCH64_ID_REGS: rust-sel4's extra() skips IDs
    // it does not know, so walk the chunks here.
    const ID_REGS: u64 = 7;
    let granule = sel4::FrameObjectType::GRANULE.bytes();
    let extra = unsafe {
        core::slice::from_raw_parts(bootinfo.ptr().cast::<u8>().add(granule), bootinfo.footprint_size() - granule)
    };
    let word = |i: usize| u64::from_le_bytes(extra[i..i + 8].try_into().unwrap());
    let mut off = 0;
    while off + 16 <= extra.len() {
        let (id, len) = (word(off), word(off + 8) as usize);
        if len < 16 || len > extra.len() - off {
            break;
        }
        if id == ID_REGS && len >= 16 + 5 * 8 {
            return core::array::from_fn(|i| word(off + 16 + 8 * i));
        }
        off += len;
    }
    [0; 5]
}

#[cfg(target_arch = "x86_64")]
fn cpu_id(_bootinfo: &sel4::BootInfoPtr) -> [u64; 5] {
    [0; 5]
}

/// All state of the root task.
pub struct Host {
    pub info: HostInfo,
    pub alloc: Alloc,
    pub wins: Windows,
    pub asids: AsidPools,
    pub pci: pci::Pci,
    pub devmem: devmem::DevMem,
    /// Disk controllers, and their units as host disks.
    pub ctrls: Vec<Controller>,
    pub disks: Vec<Disk>,
    pub netd: netd::Netd,
    pub guests: Vec<Option<Guest>>,
    pub total_bytes: usize,
    pub monitor: Option<monitor::Monitor>,
    pub dashboard: Option<dashboard::Dashboard>,
}

impl Host {
    /// Run `f` on guest `id` with root slot allocations tracked into it.
    pub fn with_guest<R>(&mut self, id: usize, f: impl FnOnce(&mut Host, &mut Guest) -> R) -> Option<R> {
        let mut g = self.guests.get_mut(id)?.take()?;
        self.alloc.slots.track(core::mem::take(&mut g.slots));
        let asids = self.asids.in_use;
        let r = f(self, &mut g);
        g.asids += self.asids.in_use - asids;
        g.slots = self.alloc.slots.untrack();
        self.guests[id] = Some(g);
        Some(r)
    }

    pub fn guest(&self, id: usize) -> Option<&Guest> {
        self.guests.get(id)?.as_ref()
    }

    /// Resources a guest operation may need, checked before starting it so
    /// that requests fail cleanly instead of exhausting the root task.
    fn can_afford(&self, bytes: usize, asids: usize) -> bool {
        bytes.checked_add(RESERVE_BYTES).is_some_and(|b| self.alloc.free_bytes() >= b)
            && self.asids.available() >= asids
            && self.alloc.slots.available() >= RESERVE_SLOTS
            && self.alloc.slots.largest_range() >= RESERVE_RANGE
    }

    pub fn create_guest(&mut self, cfg: &GuestConfig) -> Option<usize> {
        let bytes = cfg.ram.checked_add(cfg.io_pool)?;
        if !self.can_afford(bytes, cfg.vspaces + 4) {
            return None;
        }
        let id = (0..MAX_GUESTS).find(|&i| self.guest(i).is_none())?;
        if self.guests.len() <= id {
            self.guests.resize_with(id + 1, || None);
        }
        self.alloc.slots.track(Default::default());
        let asids = self.asids.in_use;
        let g = guest::create(id, cfg, &mut self.alloc, &mut self.asids, &self.info);
        let slots = self.alloc.slots.untrack();
        match g {
            Ok(mut g) => {
                g.slots = slots;
                g.asids = self.asids.in_use - asids;
                self.guests[id] = Some(g);
                Some(id)
            }
            Err(partial) => {
                // Give back whatever was built.
                self.asids.in_use = asids;
                match partial {
                    Some(mut g) => {
                        g.slots = slots;
                        g.destroy(&mut self.alloc, false);
                    }
                    None => {
                        for s in slots {
                            let _ = guest::abs(guest::root_cap::<cap_type::Null>(s)).delete();
                            self.alloc.slots.free(s);
                        }
                    }
                }
                info!("l4h-root: cannot create guest '{}': out of memory", cfg.name);
                None
            }
        }
    }

    /// Probe and start a controller once, independent of its clients.
    fn ensure_ctrl(&mut self, c: usize) -> Result<(), i64> {
        let ctrl = &self.ctrls[c];
        if ctrl.failed { return Err(ctl::EIO); }
        if ctrl.started() { return Ok(()); }
        if !self.can_afford(16 << 20, 2) { return Err(ctl::ENOMEM); }
        blkd::start(0x100 + c as u32, &mut self.ctrls[c], &self.pci,
            &mut self.devmem, &mut self.alloc, &mut self.wins.a, &mut self.asids, &self.info);
        Ok(())
    }

    /// Start the controller of host disk `disk` and learn its geometry.
    pub fn ensure_disk(&mut self, disk: usize) -> Result<(), i64> {
        let (c, unit) = self.disks.get(disk).map(|d| (d.ctrl, d.unit)).ok_or(ctl::ENOENT)?;
        self.ensure_ctrl(c)?;
        if self.disks[disk].capacity == 0 {
            let d = self.ctrls[c].info(unit).0.ok_or(ctl::EIO)?;
            self.disks[disk] = Disk { ctrl: c, ..d };
        }
        Ok(())
    }

    /// Number the host disks: one per unit of each controller, in PCI
    /// order.  virtio-scsi controllers start now, to find their logical
    /// units; the others have one unit and start when first used.
    fn scan_disks(&mut self) {
        for c in 0..self.ctrls.len() {
            if self.ctrls[c].kind != l4h_host::blk::BLKD_VIRTIO_SCSI {
                self.disks.push(Disk::new(c, 0));
                continue;
            }
            if let Err(e) = self.ensure_ctrl(c) {
                let p = &self.ctrls[c].pci;
                sel4::debug_println!("l4h-root: disk controller {:02x}:{:02x}.{} unusable: error {}", p.bus, p.dev, p.func, e);
                continue;
            }
            let units = self.ctrls[c].info(0).1;
            for u in 0..units {
                if let Some(d) = self.ctrls[c].info(u).0 {
                    self.disks.push(Disk { ctrl: c, ..d });
                }
            }
        }
    }

    /// Pass host disk `disk` (or sectors [lba_start, lba_start + lba_count)
    /// of it; 0 = to the end) through to guest `id` before it starts.
    /// The window must be aligned to blkd::WINDOW_ALIGN sectors.
    pub fn attach_disk(&mut self, id: usize, disk: usize, read_only: bool, lba_start: u64, lba_count: u64) -> Result<(), i64> {
        if lba_start % blkd::WINDOW_ALIGN != 0 || lba_count % blkd::WINDOW_ALIGN != 0 {
            return Err(ctl::EINVAL);
        }
        self.ensure_disk(disk)?;
        let (c, unit) = (self.disks[disk].ctrl, self.disks[disk].unit);
        let ctrl = &self.ctrls[c];
        let window = l4h_host::blk::DiskWindow::new(lba_start, lba_count, self.disks[disk].capacity).ok_or(ctl::EINVAL)?;
        let read_only = read_only || self.disks[disk].read_only;
        if ctrl.attachments(disk).any(|(_, a)| window.conflicts(read_only, a.window, a.read_only)) {
            return Err(ctl::EBUSY);
        }
        if ctrl.clients.iter().all(Option::is_some) { return Err(ctl::ENOSPC); }
        if !self.can_afford(1 << 20, 0) { return Err(ctl::ENOMEM); }
        self.with_guest(id, |h, g| {
            if g.state != State::Created {
                return Err(ctl::EBUSY);
            }
            let d = g.add_device(&mut h.alloc, guest::DevKind::Virtio(2)).ok_or(ctl::ENOSPC)?;
            let (client, backend) = blkd::attach(&mut h.ctrls[c], disk, g, d, window, read_only, &mut h.alloc);
            blkd::activate(&h.ctrls[c], unit, client, g, &backend, &mut h.wins.a);
            g.backends.push((d, backend));
            g.disks.push((c, client));
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
    pub fn create_nic(&mut self, id: usize, switch: u32, mac: u64, lock: bool, isolated: bool) -> Result<u64, i64> {
        if !self.can_afford(8 << 20, 0) {
            return Err(ctl::ENOMEM);
        }
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
            let (p, backend) = h.netd.add_port(g, d, switch, mac, lock, isolated, &mut h.alloc)?;
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
        // Disks are quiesced before any memory they were given is reused.
        let mut quiet = true;
        for &(ctrl, client) in &g.disks {
            quiet &= blkd::remove(&mut self.ctrls[ctrl], client, &self.pci);
        }
        let name = g.name.clone();
        self.asids.in_use -= g.asids;
        g.destroy(&mut self.alloc, !quiet);
        info!(
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
    info!("l4h-root: {} core(s), {} MiB free memory", cores, total_bytes >> 20);

    let hyp_ep = alloc.object_cap::<cap_type::Endpoint>(ObjectBlueprint::Endpoint);
    // The watchdog timer signals a notification bound to the root TCB, which
    // wakes the serve loop out of its receive on hyp_ep.
    let wd_ntfn = alloc.object_cap::<cap_type::Notification>(ObjectBlueprint::Notification);
    slot::TCB.cap().tcb_bind_notification(wd_ntfn).expect("bind watchdog notification");
    let reply = alloc.object_cap::<cap_type::Reply>(ObjectBlueprint::Reply);
    let mut wins = Windows { a: Window::new(&mut alloc, 0), b: Window::new(&mut alloc, 1) };
    let mut devmem = devmem::DevMem::from_bootinfo(bootinfo);
    let info = HostInfo {
        cores,
        tsc_hz: tsc_hz(bootinfo),
        cpu_id: cpu_id(bootinfo),
        hyp_ep,
        sched_control_base: bootinfo.sched_control().range().start,
        timeslice_us: 5000,
        epoch_ns: rtc::read_ns(&mut alloc, &mut devmem, &mut wins.a),
        epoch_ticks: rdtsc(),
        placement: core::cell::Cell::new(0),
    };
    let asids = AsidPools { pools: ::alloc::vec![(slot::ASID_POOL.cap(), 1)], in_use: 1 };

    // PCI devices: disks can be passed through to guests.
    let pci = pci::Pci::new(&mut alloc, &mut devmem);
    let mut ctrls = Vec::new();
    let mut nics = Vec::new();
    let mut balloon = None;
    for d in pci.enumerate() {
        info!(
            "l4h-root: pci {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}",
            d.bus, d.dev, d.func, d.vendor, d.device, d.class, d.subclass
        );
        if d.vendor == 0x1af4 || d.vendor == 0x1ae0 {
            let caps: Vec<u8> = d.virtio_caps.iter().map(|c| c.cfg_type).collect();
            let bars: Vec<_> = d.bars.iter().filter(|b| b.size != 0).map(|b| (b.addr, b.size, b.is_io)).collect();
            info!(
                "l4h-root:   virtio caps {:?}, msix {:?}, bars {:x?}",
                caps, d.msix.map(|m| (m.table_bar, m.table_size)), bars
            );
        }
        if blkd::is_disk(&d) {
            if ctrls.len() == pci::MAX_DISK_CTRLS {
                sel4::debug_println!("l4h-root: more than {} disk controllers, ignoring this one", pci::MAX_DISK_CTRLS);
                continue;
            }
            // Stop whatever the firmware left running: no DMA at all (the
            // memory it was given is not known), and a reset, since bus
            // mastering is enabled again on attach.
            pci.disable(&d);
            let v = pci::VECTOR_DISK_BASE + ctrls.len() as u64;
            let mut ctrl = Controller::new(d, v);
            blkd::reset(&mut ctrl, &pci, &mut alloc, &mut devmem, &mut wins.a, info.tsc_hz / 1000);
            pci.disable(&ctrl.pci);
            ctrls.push(ctrl);
        } else if netd::is_uplink(&d) {
            pci.disable(&d);
            nics.push(d);
        } else if balloon::is_balloon(&d) && balloon.is_none() {
            balloon = Some(d);
        }
    }
    // The boot disk first: dom0 takes the first DOM0_DISKS host disks.
    if let Some(bdf) = BOOT_DISK {
        match ctrls.iter().position(|c: &Controller| netd::bdf(&c.pci) == bdf) {
            Some(i) => {
                let c = ctrls.remove(i);
                ctrls.insert(0, c);
            }
            None => sel4::debug_println!("l4h-root: no boot disk at {:02x}:{:02x}.{}", bdf >> 8, (bdf >> 3) & 0x1f, bdf & 7),
        }
    }
    if let Some(d) = balloon {
        alloc.balloon = balloon::Balloon::new(&d, &pci, &mut alloc, &mut devmem, info.tsc_hz / 1000);
    }
    let mut asids = asids;
    let netd = netd::spawn(nics, &pci, &mut devmem, &mut alloc, &mut wins.a, &mut asids, &info);

    let mut h = Host {
        info,
        alloc,
        wins,
        asids,
        pci,
        devmem,
        ctrls,
        disks: Vec::new(),
        netd,
        guests: Vec::new(),
        total_bytes,
        monitor: None,
        dashboard: None,
    };
    h.scan_disks();
    h.monitor = monitor::Monitor::new(&mut h.alloc, &mut h.devmem, wd_ntfn);
    h.dashboard = dashboard::Dashboard::new(bootinfo, &mut h.alloc, &mut h.devmem);

    if DOM0_KERNEL.is_empty() {
        info!("l4h-root: no dom0 kernel embedded, nothing to do");
    } else {
        boot_dom0(&mut h);
    }
    serve(&mut h, reply)
}

pub fn boot_dom0(h: &mut Host) {
    let ram_base = elf::Elf::parse(DOM0_KERNEL).expect("dom0 kernel").bounds().0 as usize;
    let cfg = GuestConfig {
        name: "dom0",
        ram: DOM0_MEM_MB << 20,
        vcpus: DOM0_VCPUS,
        io_pool: DOM0_IO_POOL_MB << 20,
        vspaces: 64,
        ram_base,
        dom0: true,
    };
    let id = h.create_guest(&cfg).expect("cannot create dom0");
    for d in 0..h.disks.len().min(DOM0_DISKS) {
        if let Err(e) = h.attach_disk(id, d, false, 0, 0) {
            sel4::debug_println!("l4h-root: cannot give disk {} to dom0: error {}", d, e);
        }
    }
    // Switch 0, with the uplink NIC (if any; by default the first) as its
    // uplink (on first boot).
    let sw = 0;
    if h.netd.switches == 0 {
        h.netd.create_switch();
        let nic = match UPLINK_NIC {
            None => (!h.netd.nics.is_empty()).then_some(0),
            Some(bdf) => {
                let n = h.netd.nics.iter().position(|n| netd::bdf(&n.pci) == bdf);
                if n.is_none() {
                    sel4::debug_println!("l4h-root: no uplink NIC at {:02x}:{:02x}.{}", bdf >> 8, (bdf >> 3) & 0x1f, bdf & 7);
                }
                n
            }
        };
        if let Some(nic) = nic {
            h.netd.attach_uplink(sw, nic).expect("uplink");
        }
    }
    // dom0's first vNIC takes the uplink NIC's MAC address: networks
    // like GCE's only deliver frames for the instance's own address, so
    // dom0 is then the host's router (DESIGN.md §8.3).
    let uplink = h.netd.nics.iter().position(|n| n.switch == Some(sw));
    for i in 0..DOM0_NICS {
        let mac = if i == 0 { uplink.and_then(|n| h.netd.nic_mac(n)).unwrap_or(0) } else { 0 };
        h.create_nic(id, sw, mac, true, false).expect("dom0 NIC");
    }
    // The second uplink NIC, if any, on switch 1, then the internal
    // switches. dom0 finds its vNICs by MAC, not by their order.
    let uplink2 = UPLINK2_NIC.and_then(|bdf| {
        let n = h.netd.nics.iter().position(|n| netd::bdf(&n.pci) == bdf);
        if n.is_none() {
            sel4::debug_println!("l4h-root: no second uplink NIC at {:02x}:{:02x}.{}", bdf >> 8, (bdf >> 3) & 0x1f, bdf & 7);
        }
        n
    });
    if let Some(nic) = uplink2 {
        if h.netd.switches == 1 {
            h.netd.create_switch();
            h.netd.attach_uplink(1, nic).expect("second uplink");
        }
        let mac = h.netd.nic_mac(nic).unwrap_or(0);
        h.create_nic(id, 1, mac, true, false).expect("dom0 NIC");
    }
    let first = 1 + uplink2.is_some() as usize;
    while (h.netd.switches as usize) < first + DOM0_INTERNAL_SWITCHES {
        h.netd.create_switch();
    }
    for i in first..first + DOM0_INTERNAL_SWITCHES {
        h.create_nic(id, i as u32, 0, true, false).expect("dom0 internal NIC");
    }
    h.with_guest(id, |h, g| {
        assert!(
            guest::load_embedded(g, &mut h.alloc, &mut h.wins.a, &h.info, DOM0_KERNEL, DOM0_INITRD, DOM0_CMDLINE.trim()),
            "dom0 image does not fit"
        );
    });
    h.start_guest(id).expect("start dom0");
}

/// dom0 rebooted or crashed: replace it.  Other guests keep running; the
/// new dom0 finds them with the control API.
pub fn restart_dom0(h: &mut Host, id: usize) {
    sel4::debug_println!("l4h-root: restarting dom0");
    let _ = h.destroy_guest(id);
    boot_dom0(h);
}

fn rdtsc() -> u64 {
    host::ticks::now()
}

/// The root task's periodic timer: balancer and watchdog.
fn arm_timer(info: &HostInfo) {
    let deadline = rdtsc() + info.tsc_hz * sched::BALANCE_MS / 1000;
    let tcb = slot::TCB.cap();
    sel4::with_ipc_buffer_mut(|ipc| ipc.inner_mut().seL4_TCB_SetTimer(tcb.bits(), deadline, TIMER_BADGE));
}

fn watchdog_dump(h: &Host) {
    sel4::debug_println!(
        "l4h-root: watchdog: netd used {} ms since the last dump",
        h.netd.consumed() * 1000 / h.info.tsc_hz.max(1)
    );
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
        let notified = badge & (TIMER_BADGE | monitor::MONITOR_BADGE) != 0;
        if badge & monitor::MONITOR_BADGE != 0 {
            monitor::Monitor::interrupt(h);
        }
        let resp = if badge & monitor::MONITOR_BADGE != 0 && badge & TIMER_BADGE == 0 {
            None
        } else if badge & TIMER_BADGE != 0 {
            let now = rdtsc();
            sched::balance(h, (now - last_balance) * 1_000_000 / h.info.tsc_hz);
            last_balance = now;
            h.alloc.report_free(now, h.info.tsc_hz * balloon::REPORT_DELAY_MS / 1000);
            dashboard::Dashboard::tick(h);
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
                if !notified {
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
            info!("l4h-root: guest {} power request {}", id, how);
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
                info!("l4h-root: all guests stopped");
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
