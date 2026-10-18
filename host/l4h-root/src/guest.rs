//! Construction of a guest: its CSpace, guest-kernel VSpace, RAM, vCPUs and
//! boot info, following `include/l4h/abi.h`.
//!
//! A guest is created empty (`create`), gets devices attached and its
//! image loaded (`write_ram`, `copy_ram`), and is then booted
//! (`write_boot_info`, `start`).  All its objects come from its arena, so
//! `destroy` reclaims everything by revoking the arena.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr;

use sel4::init_thread::slot;
use sel4::{CNodeCapData, CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

use l4h_abi as abi;
use l4h_host as host;

use crate::component::Component;
use crate::elf::Elf;
use crate::mem::{Alloc, Arena, Dest, ObjCtx, Slot};
use crate::vmm;
use crate::vspace::{FRAME_BP, PML4_BP, map_frame};
use crate::window::{Window, Windows};
use crate::{AsidPools, HostInfo};

const PAGE: usize = 4096;
const GIB: usize = 1 << 30;
/// Devices a guest can have (console, control, virtio).
pub const MAX_GUEST_DEVICES: usize = 16;

/// Badge layout of the root fault endpoint for guest kernels.
pub const HYP_BADGE_FAULT: u64 = 1 << 24;
pub fn hyp_badge(guest: usize, vcpu: usize) -> u64 {
    (guest as u64) | ((vcpu as u64) << 16)
}
pub fn hyp_badge_guest(b: u64) -> usize {
    (b & 0xffff) as usize
}
pub fn hyp_badge_vcpu(b: u64) -> usize {
    ((b >> 16) & 0xff) as usize
}

/// Where l4h-root maps a guest's console and control pages for itself.
const ROOT_GUEST_VA: usize = 0x0000_7100_0000_0000;
fn root_guest_va(id: usize) -> usize {
    ROOT_GUEST_VA + id * 0x10_0000
}

pub struct GuestConfig<'a> {
    pub name: &'a str,
    pub ram: usize,
    pub vcpus: usize,
    pub untyped_quota: usize,
    pub vspaces: usize,
    /// Guest-virtual address of GPA 0 (from the kernel image).
    pub ram_base: usize,
    pub dom0: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevKind {
    Console,
    Ctl,
    Virtio(u32),
}

pub struct GuestDev {
    pub kind: DevKind,
    pub page: cap::Granule,
}

/// Event line of boot info device `d`.
pub fn dev_event(d: usize) -> u32 {
    16 + d as u32
}

/// A physically contiguous range of guest RAM.
#[derive(Clone, Copy, Debug)]
pub struct RamChunk {
    pub gpa: usize,
    pub paddr: usize,
    pub size: usize,
}

pub struct Vcpu {
    pub ktcb: cap::Tcb,
    pub ntfn: cap::Notification,
    pub event: cap::Granule,
    pub sc_k: cap::SchedContext,
    pub sc_u: cap::SchedContext,
    pub core: usize,
    /// Balancer periods before the vCPU may move again.
    pub cooldown: u32,
    /// CPU used in the last balancer period, in permille of a core.
    pub usage: u32,
    /// Steal-time bookkeeping (counter ticks): time since the vCPU first
    /// ran, CPU it consumed; steal published so far (ns).
    pub wall_ticks: u64,
    pub run_ticks: u64,
    pub steal_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Created,
    Running,
    Stopped,
}

pub struct Guest {
    pub id: usize,
    pub name: String,
    pub arena: Arena,
    /// Root slots allocated for this guest; cleaned up by `destroy`.
    pub slots: BTreeSet<Slot>,
    pub cnode: cap::CNode,
    pub vspace: cap::VSpace,
    pub ram_base: usize,
    pub ram: Vec<RamChunk>,
    /// RAM CNodes and their radix (bits of frame index they resolve).
    pub ram_cnodes: Vec<(cap::CNode, usize)>,
    pub vcpus: Vec<Vcpu>,
    pub state: State,
    pub dom0: bool,
    untyped: Vec<usize>,
    vspaces: usize,
    bootinfo: Vec<cap::Granule>,
    entry: u64,
    /// Objects l4h-vmm serves the guest with: its endpoint (the guest's
    /// HYP_EP), its bound notification (device kicks) and the device pages.
    pub vmm_ep: cap::Endpoint,
    pub vmm_ntfn: cap::Notification,
    dev_cn: cap::CNode,
    pub devices: Vec<GuestDev>,
    /// Host side of each virtio device, by device index.
    pub backends: Vec<(usize, vmm::Backend)>,
    pub console_ring: Vec<cap::Granule>,
    pub vmm: Option<Component>,
    /// CPU cap per vCPU in percent of a core (0 = none).
    pub cpu_cap_pct: u32,
    /// Disks (host disk indices) attached to the guest.
    pub disks: Vec<usize>,
    /// netd ports of its virtio-net devices.
    pub ports: Vec<usize>,
    /// ASIDs held by its VSpaces (its own, its processes', its components').
    pub asids: usize,
}

pub(crate) fn abs(c: impl sel4::HasCPtrWithDepth) -> sel4::AbsoluteCPtr {
    slot::CNODE.cap().absolute_cptr(c)
}

pub(crate) fn root_cap<T: sel4::CapType>(s: Slot) -> sel4::Cap<T> {
    sel4::init_thread::Slot::<T>::from_index(s).cap()
}

/// Radix of the RAM CNode covering GiB `n` of a guest with `ram` bytes.
fn ram_cnode_radix(ram: usize, n: usize) -> usize {
    let frames = (ram - n * GIB).min(GIB) / PAGE;
    (frames.next_power_of_two().trailing_zeros() as usize).max(1)
}

impl Guest {
    pub fn ctx<'a>(&'a mut self, alloc: &'a mut Alloc) -> ObjCtx<'a> {
        ObjCtx { slots: &mut alloc.slots, pool: &mut self.arena.pool }
    }

    pub fn ram_size(&self) -> usize {
        self.ram.iter().map(|c| c.size).sum()
    }

    /// Destination for a capability in the guest's root CNode.
    fn guest_slot(&self, idx: usize) -> sel4::AbsoluteCPtr {
        self.cnode.absolute_cptr_from_bits_with_depth(idx as u64, abi::ROOT_CNODE_BITS)
    }

    fn give(&self, idx: usize, src: impl sel4::HasCPtrWithDepth, rights: CapRights) {
        self.guest_slot(idx)
            .copy(&abs(src), rights)
            .unwrap_or_else(|e| panic!("guest slot {} copy failed: {:?}", idx, e));
    }

    fn give_badged(&self, idx: usize, src: impl sel4::HasCPtrWithDepth, rights: CapRights, badge: u64) {
        self.guest_slot(idx)
            .mint(&abs(src), rights, badge)
            .unwrap_or_else(|e| panic!("guest slot {} mint failed: {:?}", idx, e));
    }

    /// The frame capability backing guest-physical address `gpa`.
    pub fn ram_frame(&self, gpa: usize) -> sel4::AbsoluteCPtr {
        let (cn, radix) = self.ram_cnodes[gpa / GIB];
        cn.absolute_cptr_from_bits_with_depth(((gpa / PAGE) % abi::RAM_CNODE_FRAMES) as u64, radix)
    }

    pub fn valid_gpa(&self, gpa: usize, len: usize) -> bool {
        gpa.checked_add(len).is_some_and(|end| end <= self.ram_size())
    }

    /// Copy `data` into guest RAM at `gpa`.
    pub fn write_ram(&self, win: &mut Window, gpa: usize, data: &[u8]) {
        assert!(self.valid_gpa(gpa, data.len()));
        let mut off = 0;
        while off < data.len() {
            let page = (gpa + off) & !(PAGE - 1);
            let in_page = (gpa + off) - page;
            let n = (PAGE - in_page).min(data.len() - off);
            win.with_frame(&self.ram_frame(page), |p| unsafe {
                ptr::copy_nonoverlapping(data[off..].as_ptr(), p.add(in_page), n);
            });
            off += n;
        }
    }

    /// Copy `len` bytes from `src` RAM at `src_gpa` to ours at `gpa`.
    pub fn copy_ram(&self, wins: &mut Windows, gpa: usize, src: &Guest, src_gpa: usize, len: usize) -> bool {
        if !self.valid_gpa(gpa, len) || !src.valid_gpa(src_gpa, len) {
            return false;
        }
        let mut off = 0;
        while off < len {
            let (s, d) = (src_gpa + off, gpa + off);
            let n = (PAGE - s % PAGE).min(PAGE - d % PAGE).min(len - off);
            let (sp, dp) = (s & !(PAGE - 1), d & !(PAGE - 1));
            let (wa, wb) = (&mut wins.a, &mut wins.b);
            wa.with_frame(&src.ram_frame(sp), |from| {
                wb.with_frame(&self.ram_frame(dp), |to| unsafe {
                    ptr::copy_nonoverlapping(from.add(s - sp), to.add(d - dp), n);
                })
            });
            off += n;
        }
        true
    }

    /// Add a device: a shared page mapped in the guest at its device area
    /// and a kick capability (badged notification of l4h-vmm).
    pub fn add_device(&mut self, alloc: &mut Alloc, kind: DevKind) -> Option<usize> {
        let d = self.devices.len();
        if d >= MAX_GUEST_DEVICES || self.state != State::Created {
            return None;
        }
        let page = self.arena.object_cap::<cap_type::Granule>(&mut alloc.slots, FRAME_BP);
        let (vspace, ntfn) = (self.vspace, self.vmm_ntfn);
        let mut ctx = self.ctx(alloc);
        // The guest maps a copy; the original is kept for l4h-vmm.
        let copy = ctx.slots.alloc();
        abs(root_cap::<cap_type::Null>(copy)).copy(&abs(page), CapRights::all()).expect("device page copy");
        map_frame(&mut ctx, vspace, root_cap::<cap_type::Granule>(copy), abi::dev_area_va(d) as usize, CapRights::read_write(), VmAttributes::DEFAULT);
        self.dev_cn
            .absolute_cptr_from_bits_with_depth(d as u64, abi::ROOT_CNODE_BITS)
            .mint(&abs(ntfn), CapRights::write_only(), 1 << (host::VMM_BADGE_KICK_SHIFT as usize + d))
            .expect("device kick cap");
        // Console and control pages are also used by l4h-root itself.
        let root_va = match kind {
            DevKind::Ctl => Some(root_guest_va(self.id)),
            DevKind::Console => Some(root_guest_va(self.id) + PAGE),
            DevKind::Virtio(_) => None,
        };
        if let Some(va) = root_va {
            self.map_in_root(alloc, page, va);
        }
        self.devices.push(GuestDev { kind, page });
        Some(d)
    }

    /// Map (a copy of) one of our frames into the root task.  The paging
    /// structures are global: the root region is reused by later guests.
    fn map_in_root(&mut self, alloc: &mut Alloc, frame: cap::Granule, va: usize) {
        let copy = alloc.slots.alloc();
        abs(root_cap::<cap_type::Null>(copy)).copy(&abs(frame), CapRights::all()).expect("root map copy");
        let Alloc { slots, pool, .. } = alloc;
        slots.global(|slots| {
            map_frame(&mut ObjCtx { slots, pool }, slot::VSPACE.cap(), root_cap::<cap_type::Granule>(copy), va, CapRights::read_write(), VmAttributes::DEFAULT);
        });
    }

    pub fn ctl_page(&self) -> Option<*mut u8> {
        self.devices.iter().any(|d| d.kind == DevKind::Ctl).then(|| root_guest_va(self.id) as *mut u8)
    }

    pub fn console_page(&self) -> *mut abi::ConsolePage {
        (root_guest_va(self.id) + PAGE) as *mut abi::ConsolePage
    }

    /// Raise event line `ev` on vCPU 0, as l4h-vmm does for devices.
    pub fn raise_event(&self, alloc: &mut Alloc, ev: u32) {
        let page = self.event_page(0);
        unsafe {
            let w = core::ptr::addr_of_mut!((*page).pending[(ev / 64) as usize]) as *const core::sync::atomic::AtomicU64;
            (*w).fetch_or(1 << (ev % 64), core::sync::atomic::Ordering::SeqCst);
        }
        let s = alloc.slots.alloc();
        let badged = root_cap::<cap_type::Notification>(s);
        abs(badged).mint(&abs(self.vcpus[0].ntfn), CapRights::write_only(), abi::BADGE_EVENT).expect("event cap");
        badged.signal();
        abs(badged).delete().expect("delete event cap");
        alloc.slots.free(s);
    }

    /// Event page of vCPU `v`, as mapped in the root task.
    pub fn event_page(&self, v: usize) -> *mut abi::EventPage {
        (root_guest_va(self.id) + 0x2_0000 + v * PAGE) as *mut abi::EventPage
    }

    pub fn console_ring(&self) -> *const host::ConsoleRing {
        (root_guest_va(self.id) + 0x1_0000) as *const host::ConsoleRing
    }

    /// Write the boot info.  Validates the boot parameters.
    pub fn write_boot_info(
        &mut self,
        win: &mut Window,
        host_info: &HostInfo,
        entry: u64,
        kernel_end: usize,
        initrd: (usize, usize),
        cmdline: &[u8],
    ) -> bool {
        let ram = self.ram_size();
        let entry_gpa = (entry as usize).wrapping_sub(self.ram_base);
        if kernel_end > ram || kernel_end % PAGE != 0 || entry_gpa >= kernel_end || !self.valid_gpa(initrd.0, initrd.1) {
            return false;
        }
        let mut bi: alloc::boxed::Box<abi::BootInfo> = unsafe { alloc::boxed::Box::new_zeroed().assume_init() };
        bi.magic = abi::BOOTINFO_MAGIC;
        bi.abi_version = abi::ABI_VERSION;
        bi.guest_id = self.id as u64;
        bi.ram_base_va = self.ram_base as u64;
        bi.ram_size = ram as u64;
        bi.kernel_end_gpa = kernel_end as u64;
        if initrd.1 != 0 {
            bi.initrd_gpa = initrd.0 as u64;
            bi.initrd_size = initrd.1 as u64;
        }
        bi.nr_vcpus = self.vcpus.len() as u64;
        bi.tsc_hz = host_info.tsc_hz;
        bi.nr_untyped = self.untyped.len() as u64;
        for (i, b) in self.untyped.iter().enumerate() {
            bi.untyped[i].size_bits = *b as u64;
        }
        bi.nr_vspaces = self.vspaces as u64;
        bi.flags = if self.dom0 { abi::BOOT_DOM0 } else { 0 };
        bi.nr_devices = self.devices.len() as u64;
        for (d, dev) in self.devices.iter().enumerate() {
            let (ty, virtio_id) = match dev.kind {
                DevKind::Console => (abi::DEV_CONSOLE, 0),
                DevKind::Ctl => (abi::DEV_CTL, 0),
                DevKind::Virtio(id) => (abi::DEV_VIRTIO, id),
            };
            bi.devices[d] = abi::DevDesc {
                ty,
                virtio_id,
                page_va: abi::dev_area_va(d),
                page_size: PAGE as u64,
                event: dev_event(d),
                kick_slot: d as u32,
                reserved: [0; 2],
            };
        }
        let n = cmdline.iter().position(|&c| c == 0).unwrap_or(cmdline.len()).min(abi::CMDLINE_SIZE - 1);
        bi.cmdline[..n].copy_from_slice(&cmdline[..n]);

        let bi_bytes = unsafe {
            core::slice::from_raw_parts(&*bi as *const abi::BootInfo as *const u8, core::mem::size_of::<abi::BootInfo>())
        };
        for (p, frame) in self.bootinfo.iter().enumerate() {
            let lo = (p * PAGE).min(bi_bytes.len());
            let hi = ((p + 1) * PAGE).min(bi_bytes.len());
            let chunk = &bi_bytes[lo..hi];
            win.with_frame(&abs(*frame), |dst| unsafe {
                ptr::write_bytes(dst, 0, PAGE);
                ptr::copy_nonoverlapping(chunk.as_ptr(), dst, chunk.len());
            });
        }
        self.entry = entry;
        true
    }

    /// Start vCPU 0 at the kernel entry point.
    pub fn start(&mut self) {
        let mut regs = sel4::UserContext::default();
        *regs.pc_mut() = self.entry;
        *regs.c_param_mut(0) = abi::BOOTINFO_VA;
        self.vcpus[0].ktcb.tcb_write_all_registers(true, &mut regs).expect("start vCPU 0");
        self.state = State::Running;
        sel4::debug_println!("l4h-root: guest {} '{}' started", self.id, self.name);
    }

    /// Stop all threads of the guest and its VMM.
    pub fn stop(&mut self) {
        for v in &self.vcpus {
            let _ = v.ktcb.tcb_suspend();
        }
        if let Some(v) = &self.vmm {
            let _ = v.tcb.tcb_suspend();
        }
        self.state = State::Stopped;
    }

    /// Destroy the guest and return all its memory and root slots.  The
    /// caller has detached its devices.
    pub fn destroy(mut self, alloc: &mut Alloc) {
        self.stop();
        alloc.reclaim(self.arena);
        // Capabilities that do not derive from the arena (badged endpoints,
        // copies of global objects) are left in their slots.
        for s in self.slots {
            let _ = abs(root_cap::<cap_type::Null>(s)).delete();
            alloc.slots.free(s);
        }
    }
}

/// Create a guest with no image.  Root slots allocated here and in later
/// operations on the guest must be tracked into `Guest::slots` by the
/// caller (see `Alloc::slots`).  On failure, returns the partly built
/// guest, if there is one, for the caller to destroy.
pub fn create(
    id: usize,
    cfg: &GuestConfig,
    alloc: &mut Alloc,
    asids: &mut AsidPools,
    host_info: &HostInfo,
) -> Result<Guest, Option<Guest>> {
    // Every value comes from the control API: check before computing.
    if cfg.vcpus < 1 || cfg.vcpus > abi::MAX_VCPUS || cfg.ram < (8 << 20) || cfg.ram > abi::MAX_RAM_CNODES * GIB {
        return Err(None);
    }
    if cfg.untyped_quota > abi::MAX_RAM_CNODES * GIB || cfg.ram_base % PAGE != 0 {
        return Err(None);
    }
    let ram_base = cfg.ram_base;
    let ram = (cfg.ram + PAGE - 1) & !(PAGE - 1);
    if ram_base == 0 || ram_base.checked_add(ram).is_none_or(|end| end as u64 > abi::SPECIAL_BASE) {
        return Err(None);
    }

    // Host-side objects for this guest: CNodes, paging structures for the
    // direct map, vCPU objects, VSpace pool, boot info, its l4h-vmm.
    // Power-of-two splitting and alignment can waste up to half, hence the
    // factor 2.
    let n_cnodes = ram.div_ceil(GIB);
    let ram_cnode_bytes: usize = (0..n_cnodes).map(|n| ram_cnode_radix(ram, n)).map(|radix| 32usize << radix).sum();
    let overhead = 2 * (ram_cnode_bytes
        + (32 << abi::ROOT_CNODE_BITS) * 3
        + ram / 256
        + cfg.vcpus * (64 << 10)
        + cfg.vspaces * PAGE
        + (2 << 20))
        + vmm::arena_bytes(ram, cfg.vcpus, MAX_GUEST_DEVICES);
    if alloc.free_bytes() < overhead + ram + cfg.untyped_quota {
        return Err(None);
    }
    let mut arena = alloc.arena(overhead).ok_or(None)?;

    // Guest root CNode.
    let cnode_slot = alloc.slots.alloc();
    arena
        .pool
        .retype(&ObjectBlueprint::CNode { size_bits: abi::ROOT_CNODE_BITS }, 1, Dest::root(cnode_slot))
        .expect("guest CNode");
    let vspace_slot = arena.object(&mut alloc.slots, PML4_BP);
    let vspace = root_cap::<cap_type::VSpace>(vspace_slot);
    asids.assign(alloc, vspace);

    let mut g = Guest {
        id,
        name: String::from(cfg.name),
        arena,
        slots: BTreeSet::new(),
        cnode: root_cap::<cap_type::CNode>(cnode_slot),
        vspace,
        ram_base,
        ram: Vec::new(),
        ram_cnodes: Vec::new(),
        vcpus: Vec::new(),
        state: State::Created,
        dom0: cfg.dom0,
        untyped: Vec::new(),
        vspaces: cfg.vspaces,
        bootinfo: Vec::new(),
        entry: 0,
        vmm_ep: cap::Endpoint::from_bits(0),
        vmm_ntfn: cap::Notification::from_bits(0),
        dev_cn: cap::CNode::from_bits(0),
        devices: Vec::new(),
        backends: Vec::new(),
        console_ring: Vec::new(),
        vmm: None,
        cpu_cap_pct: 0,
        disks: Vec::new(),
        ports: Vec::new(),
        asids: 0,
    };

    let guard34 = CNodeCapData::new(0, abi::ROOT_GUARD_BITS).into_word();
    let guard6 = CNodeCapData::new(0, abi::L2_INDEX_BITS - abi::ROOT_CNODE_BITS).into_word();
    let meta = g.arena.object_cap::<cap_type::CNode>(&mut alloc.slots, ObjectBlueprint::CNode { size_bits: abi::ROOT_CNODE_BITS });
    meta.absolute_cptr_from_bits_with_depth(0, abi::ROOT_CNODE_BITS)
        .mint(&abs(g.cnode), CapRights::all(), guard34)
        .expect("meta CNode");
    g.give_badged(abi::SLOT_META, meta, CapRights::all(), guard6);
    g.give(abi::SLOT_VSPACE, g.vspace, CapRights::all());

    // ---- RAM: physically contiguous chunks retyped into 4 KiB frames that
    // live in CNodes, one per GiB of guest-physical space.
    for n in 0..n_cnodes {
        let radix = ram_cnode_radix(ram, n);
        let s = alloc.slots.alloc();
        g.arena
            .pool
            .retype(&ObjectBlueprint::CNode { size_bits: radix }, 1, Dest::root(s))
            .expect("RAM CNode");
        let cn = root_cap::<cap_type::CNode>(s);
        let guard = CNodeCapData::new(0, abi::L2_INDEX_BITS - radix).into_word();
        g.give_badged(abi::SLOT_RAM_CNODE_BASE + n, cn, CapRights::all(), guard);
        g.ram_cnodes.push((cn, radix));
    }
    let mut gpa = 0;
    while gpa < ram {
        // l4h-vmm and l4h-blkd take at most this many chunks.
        if g.ram.len() == host::VMM_MAX_RAM_CHUNKS {
            return Err(Some(g));
        }
        let remaining = ram - gpa;
        // Largest power-of-two chunk that fits, never crossing a GiB.
        let mut bits = (usize::BITS - 1 - remaining.leading_zeros()) as usize;
        bits = bits.min(30);
        let chunk_slot = alloc.slots.alloc();
        let region = loop {
            if let Some(r) = alloc.split(bits, chunk_slot) {
                break r;
            }
            if bits <= 21 {
                return Err(Some(g));
            }
            bits -= 1;
        };
        let size = 1usize << bits;
        let frames = size / PAGE;
        let (cn, _) = g.ram_cnodes[gpa / GIB];
        let dest = Dest { cnode: abs(cn), offset: (gpa / PAGE) % abi::RAM_CNODE_FRAMES };
        let paddr = region.paddr;
        let mut pool = crate::mem::UtPool { regions: ::alloc::vec![region] };
        pool.retype(&FRAME_BP, frames, dest).expect("RAM frames");
        g.arena.own(chunk_slot, paddr, bits);
        g.ram.push(RamChunk { gpa, paddr, size });
        gpa += size;
    }

    // ---- Direct map of all guest RAM in the guest-kernel VSpace.  The
    // original frame caps are used; the guest maps copies elsewhere.
    let tmp = alloc.slots.alloc();
    for page in (0..ram).step_by(PAGE) {
        abs(root_cap::<cap_type::Null>(tmp)).move_(&g.ram_frame(page)).expect("move frame");
        let frame = root_cap::<cap_type::Granule>(tmp);
        let vspace = g.vspace;
        map_frame(&mut g.ctx(alloc), vspace, frame, ram_base + page, CapRights::read_write(), VmAttributes::DEFAULT);
        // Put it back so the guest finds it at its ABI location.
        g.ram_frame(page).move_(&abs(frame)).expect("move frame back");
    }
    alloc.slots.free(tmp);

    // ---- Untyped quota for the guest's own kernel objects.
    let mut remaining = cfg.untyped_quota;
    while remaining >= PAGE && g.untyped.len() < abi::MAX_UNTYPED {
        let bits = (usize::BITS - 1 - remaining.leading_zeros()) as usize;
        let bits = bits.min(28);
        let s = alloc.slots.alloc();
        let Some(r) = alloc.split(bits, s) else {
            alloc.slots.free(s);
            break;
        };
        g.arena.own(s, r.paddr, bits);
        g.give(abi::SLOT_UNTYPED_BASE + g.untyped.len(), root_cap::<cap_type::Untyped>(s), CapRights::all());
        g.untyped.push(bits);
        remaining -= 1 << bits;
    }

    // ---- VSpace pool for guest processes and device CNode.
    let pool_cn = g.arena.object_cap::<cap_type::CNode>(&mut alloc.slots, ObjectBlueprint::CNode { size_bits: abi::ROOT_CNODE_BITS });
    for i in 0..cfg.vspaces {
        let vs = g.arena.object_cap::<cap_type::VSpace>(&mut alloc.slots, PML4_BP);
        asids.assign(alloc, vs);
        pool_cn
            .absolute_cptr_from_bits_with_depth(i as u64, abi::ROOT_CNODE_BITS)
            .copy(&abs(vs), CapRights::all())
            .expect("vspace pool copy");
    }
    g.give_badged(abi::SLOT_VSPACE_POOL, pool_cn, CapRights::all(), guard6);
    g.dev_cn = g.arena.object_cap::<cap_type::CNode>(&mut alloc.slots, ObjectBlueprint::CNode { size_bits: abi::ROOT_CNODE_BITS });
    g.give_badged(abi::SLOT_DEV_CNODE, g.dev_cn, CapRights::all(), guard6);

    // ---- Hypercall endpoint (served by l4h-vmm).
    g.vmm_ep = g.arena.object_cap::<cap_type::Endpoint>(&mut alloc.slots, ObjectBlueprint::Endpoint);
    g.vmm_ntfn = g.arena.object_cap::<cap_type::Notification>(&mut alloc.slots, ObjectBlueprint::Notification);
    g.give_badged(
        abi::SLOT_HYP_EP,
        g.vmm_ep,
        sel4::CapRightsBuilder::none().write(true).grant_reply(true).build(),
        host::VMM_BADGE_HYPERCALL,
    );

    // ---- vCPUs.
    for v in 0..cfg.vcpus {
        let core = (host_info.next_core() + v) % host_info.cores;
        let mut ctx = ObjCtx { slots: &mut alloc.slots, pool: &mut g.arena.pool };
        let ktcb = ctx.cap::<cap_type::Tcb>(ObjectBlueprint::Tcb);
        let ep = ctx.cap::<cap_type::Endpoint>(ObjectBlueprint::Endpoint);
        let ntfn = ctx.cap::<cap_type::Notification>(ObjectBlueprint::Notification);
        let sc_bits = sel4::sys::seL4_MinSchedContextBits as usize;
        let sc_k = ctx.cap::<cap_type::SchedContext>(ObjectBlueprint::SchedContext { size_bits: sc_bits });
        let sc_u = ctx.cap::<cap_type::SchedContext>(ObjectBlueprint::SchedContext { size_bits: sc_bits });
        let ipcbuf = ctx.cap::<cap_type::Granule>(FRAME_BP);
        let event = ctx.cap::<cap_type::Granule>(FRAME_BP);
        let vspace = g.vspace;
        map_frame(&mut ctx, vspace, ipcbuf, abi::vcpu_ipcbuf_va(v) as usize, CapRights::read_write(), VmAttributes::DEFAULT);
        map_frame(&mut ctx, vspace, event, abi::vcpu_event_va(v) as usize, CapRights::read_write(), VmAttributes::DEFAULT);

        let sched_control = host_info.sched_control(core);
        sched_control
            .sched_control_configure_flags(sc_k, host_info.timeslice_us, host_info.timeslice_us, 0, 0, 0)
            .expect("configure SC_K");
        sched_control
            .sched_control_configure_flags(sc_u, host_info.timeslice_us, host_info.timeslice_us, 0, 0, 0)
            .expect("configure SC_U");

        ktcb.tcb_configure(g.cnode, CNodeCapData::new(0, abi::ROOT_GUARD_BITS), g.vspace, abi::vcpu_ipcbuf_va(v), ipcbuf)
            .expect("kTCB configure");
        let fault_slot = alloc.slots.alloc();
        abs(root_cap::<cap_type::Null>(fault_slot))
            .mint(&abs(host_info.hyp_ep), CapRights::all(), hyp_badge(id, v) | HYP_BADGE_FAULT)
            .expect("fault ep");
        ktcb.tcb_set_sched_params(
            slot::TCB.cap(),
            abi::PRIO_KERNEL,
            abi::PRIO_KERNEL,
            sc_k,
            root_cap::<cap_type::Endpoint>(fault_slot),
        )
        .expect("kTCB sched params");
        ktcb.tcb_bind_notification(ntfn).expect("bind notification");

        let base = abi::slot_vcpu(v);
        g.give(base + abi::VCPU_KTCB, ktcb, CapRights::all());
        g.give(base + abi::VCPU_EP, ep, CapRights::all());
        g.give(base + abi::VCPU_NTFN, ntfn, CapRights::all());
        g.give(base + abi::VCPU_SC_U, sc_u, CapRights::all());
        g.give_badged(base + abi::VCPU_IPI, ntfn, CapRights::write_only(), abi::BADGE_IPI);
        g.give(base + abi::VCPU_IPCBUF, ipcbuf, CapRights::read_write());
        g.give(base + abi::VCPU_EVENT, event, CapRights::read_write());
        g.vcpus.push(Vcpu { ktcb, ntfn, event, sc_k, sc_u, core, cooldown: 0, usage: 0, wall_ticks: 0, run_ticks: 0, steal_ns: 0 });
    }

    // ---- Event pages in the root task too (steal time).
    for v in 0..g.vcpus.len() {
        let ev = g.vcpus[v].event;
        g.map_in_root(alloc, ev, root_guest_va(id) + 0x2_0000 + v * PAGE);
    }

    // ---- Boot info frames (written at boot).
    for p in 0..(abi::BOOTINFO_SIZE as usize / PAGE) {
        let frame = g.arena.object_cap::<cap_type::Granule>(&mut alloc.slots, FRAME_BP);
        let vspace = g.vspace;
        map_frame(&mut g.ctx(alloc), vspace, frame, abi::BOOTINFO_VA as usize + p * PAGE, CapRights::read_only(), VmAttributes::DEFAULT);
        g.bootinfo.push(frame);
    }

    // ---- Console (input page, output ring) and, for dom0, control.
    g.add_device(alloc, DevKind::Console).unwrap();
    for i in 0..host::CONSOLE_RING_PAGES {
        let f = g.arena.object_cap::<cap_type::Granule>(&mut alloc.slots, FRAME_BP);
        let va = root_guest_va(id) + 0x1_0000 + i * PAGE;
        g.map_in_root(alloc, f, va);
        g.console_ring.push(f);
    }
    if cfg.dom0 {
        g.add_device(alloc, DevKind::Ctl).unwrap();
    }

    sel4::debug_println!(
        "l4h-root: guest {} '{}' created: {} MiB RAM in {} chunk(s), {} vCPU(s), {} MiB untyped",
        id, g.name, ram >> 20, g.ram.len(), cfg.vcpus,
        g.untyped.iter().map(|b| 1usize << b).sum::<usize>() >> 20
    );
    Ok(g)
}

/// Load an ELF kernel and initrd embedded in the root task (dom0) and write
/// the boot info.  Returns false if the image does not fit.
pub fn load_embedded(g: &mut Guest, win: &mut Window, host_info: &HostInfo, kernel: &[u8], initrd: &[u8], cmdline: &str) -> bool {
    let Ok(elf) = Elf::parse(kernel) else { return false };
    let (lo, hi) = elf.bounds();
    if (lo as usize) & !(PAGE - 1) != g.ram_base {
        return false;
    }
    let kernel_end = ((hi as usize - g.ram_base) + PAGE - 1) & !(PAGE - 1);
    let initrd_gpa = kernel_end + (1 << 20);
    if !g.valid_gpa(initrd_gpa, initrd.len() + (1 << 20)) {
        return false;
    }
    for seg in elf.segments() {
        g.write_ram(win, seg.vaddr as usize - g.ram_base, seg.data);
    }
    if !initrd.is_empty() {
        g.write_ram(win, initrd_gpa, initrd);
    }
    g.write_boot_info(win, host_info, elf.entry, kernel_end, (initrd_gpa, initrd.len()), cmdline.as_bytes())
}
