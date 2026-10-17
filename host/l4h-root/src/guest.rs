//! Construction of a guest: its CSpace, guest-kernel VSpace, RAM, vCPUs and
//! boot info, following `include/l4h/abi.h`.

use alloc::string::String;
use alloc::vec::Vec;
use core::ptr;

use sel4::init_thread::slot;
use sel4::{CNodeCapData, CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

use l4h_abi as abi;

use crate::mem::{Alloc, Arena, Dest, ObjCtx, Slot};
use crate::elf::Elf;
use crate::vspace::{FRAME_BP, PML4_BP, map_frame};
use crate::window::Window;
use crate::{AsidPools, HostInfo};

const PAGE: usize = 4096;
const GIB: usize = 1 << 30;

/// Badge layout of the root hypercall/fault endpoint.
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

pub struct GuestConfig<'a> {
    pub name: &'a str,
    pub ram: usize,
    pub vcpus: usize,
    pub untyped_quota: usize,
    pub vspaces: usize,
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    pub cmdline: &'a str,
    pub dom0: bool,
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
    pub core: usize,
}

pub struct Guest {
    pub id: usize,
    pub name: String,
    pub arena: Arena,
    pub cnode: cap::CNode,
    pub vspace: cap::VSpace,
    pub ram: Vec<RamChunk>,
    /// RAM CNodes and their radix (bits of frame index they resolve).
    pub ram_cnodes: Vec<(cap::CNode, usize)>,
    pub vcpus: Vec<Vcpu>,
    pub running: bool,
}

fn abs(c: impl sel4::HasCPtrWithDepth) -> sel4::AbsoluteCPtr {
    slot::CNODE.cap().absolute_cptr(c)
}

fn root_cap<T: sel4::CapType>(s: Slot) -> sel4::Cap<T> {
    sel4::init_thread::Slot::<T>::from_index(s).cap()
}

impl Guest {
    fn ctx<'a>(&'a mut self, alloc: &'a mut Alloc) -> ObjCtx<'a> {
        ObjCtx { slots: &mut alloc.slots, pool: &mut self.arena.pool }
    }

    /// Destination for a capability in the guest's root CNode.
    fn guest_slot(&self, idx: usize) -> sel4::AbsoluteCPtr {
        self.cnode
            .absolute_cptr_from_bits_with_depth(idx as u64, abi::ROOT_CNODE_BITS)
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
    fn ram_frame(&self, gpa: usize) -> sel4::AbsoluteCPtr {
        let (cn, radix) = self.ram_cnodes[gpa / GIB];
        cn.absolute_cptr_from_bits_with_depth(((gpa / PAGE) % abi::RAM_CNODE_FRAMES) as u64, radix)
    }

    /// Copy `data` into guest RAM at `gpa`.
    fn write_ram(&self, win: &mut Window, alloc: &mut Alloc, gpa: usize, data: &[u8]) {
        let mut off = 0;
        while off < data.len() {
            let page = (gpa + off) & !(PAGE - 1);
            let in_page = (gpa + off) - page;
            let n = (PAGE - in_page).min(data.len() - off);
            win.with_frame(alloc, &self.ram_frame(page), |p| unsafe {
                ptr::copy_nonoverlapping(data[off..].as_ptr(), p.add(in_page), n);
            });
            off += n;
        }
    }
}

/// Radix of the RAM CNode covering GiB `n` of a guest with `ram` bytes.
fn ram_cnode_radix(ram: usize, n: usize) -> usize {
    let frames = (ram - n * GIB).min(GIB) / PAGE;
    (frames.next_power_of_two().trailing_zeros() as usize).max(1)
}

pub fn build(
    id: usize,
    cfg: &GuestConfig,
    alloc: &mut Alloc,
    win: &mut Window,
    asids: &mut AsidPools,
    host: &HostInfo,
) -> Guest {
    let elf = Elf::parse(cfg.kernel).expect("guest kernel");
    let (lo, hi) = elf.bounds();
    let ram_base = (lo as usize) & !(PAGE - 1);
    let kernel_end = ((hi as usize - ram_base) + PAGE - 1) & !(PAGE - 1);
    let ram = (cfg.ram + PAGE - 1) & !(PAGE - 1);
    assert!(cfg.vcpus >= 1 && cfg.vcpus <= abi::MAX_VCPUS);
    assert!(ram <= abi::MAX_RAM_CNODES * GIB);
    assert!(kernel_end + cfg.initrd.len() + (1 << 20) < ram, "guest RAM too small for kernel+initrd");
    assert!((ram_base + ram) as u64 <= abi::SPECIAL_BASE);

    // Host-side objects for this guest: CNodes, paging structures for the
    // direct map, vCPU objects, VSpace pool, boot info.  Power-of-two
    // splitting and alignment can waste up to half, hence the factor 2.
    let n_cnodes = ram.div_ceil(GIB);
    let ram_cnode_bytes: usize = (0..n_cnodes)
        .map(|n| ram_cnode_radix(ram, n))
        .map(|radix| 32usize << radix)
        .sum();
    let overhead = 2 * (ram_cnode_bytes
        + (32 << abi::ROOT_CNODE_BITS) * 3
        + ram / 256
        + cfg.vcpus * (64 << 10)
        + cfg.vspaces * PAGE
        + (2 << 20));
    let arena = alloc.arena(overhead).expect("out of memory for guest arena");

    // Guest root CNode.
    let cnode_slot = alloc.slots.alloc();
    let mut arena = arena;
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
        cnode: root_cap::<cap_type::CNode>(cnode_slot),
        vspace,
        ram: Vec::new(),
        ram_cnodes: Vec::new(),
        vcpus: Vec::new(),
        running: false,
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
    // live in radix-18 CNodes, one per GiB of guest-physical space.
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
        let remaining = ram - gpa;
        // Largest power-of-two chunk that fits, never crossing a GiB.
        let mut bits = (usize::BITS - 1 - remaining.leading_zeros()) as usize;
        bits = bits.min(30);
        let chunk_slot = alloc.slots.alloc();
        let region = loop {
            if let Some(r) = alloc.pool.split(bits, chunk_slot) {
                break r;
            }
            assert!(bits > 21, "out of memory for guest RAM");
            bits -= 1;
        };
        let size = 1usize << bits;
        let frames = size / PAGE;
        let (cn, _) = g.ram_cnodes[gpa / GIB];
        let dest = Dest {
            cnode: abs(cn),
            offset: (gpa / PAGE) % abi::RAM_CNODE_FRAMES,
        };
        let mut pool = crate::mem::UtPool { regions: ::alloc::vec![region] };
        pool.retype(&FRAME_BP, frames, dest).expect("RAM frames");
        let paddr = pool.regions[0].paddr;
        g.arena.slots.push(chunk_slot);
        g.ram.push(RamChunk { gpa, paddr, size });
        gpa += size;
    }

    // ---- Direct map of all guest RAM in the guest-kernel VSpace.  The
    // original frame caps are used; the guest maps copies elsewhere.
    for page in (0..ram).step_by(PAGE) {
        let s = alloc.slots.alloc();
        abs(root_cap::<cap_type::Null>(s)).move_(&g.ram_frame(page)).expect("move frame");
        let frame = root_cap::<cap_type::Granule>(s);
        let vspace = g.vspace;
        map_frame(&mut g.ctx(alloc), vspace, frame, ram_base + page, CapRights::read_write(), VmAttributes::DEFAULT);
        // Put it back so the guest finds it at its ABI location.
        g.ram_frame(page).move_(&abs(frame)).expect("move frame back");
        alloc.slots.free(s);
    }

    // ---- Kernel image and initrd.
    for seg in elf.segments() {
        g.write_ram(win, alloc, seg.vaddr as usize - ram_base, seg.data);
    }
    let initrd_gpa = kernel_end + (1 << 20);
    if !cfg.initrd.is_empty() {
        g.write_ram(win, alloc, initrd_gpa, cfg.initrd);
    }

    // ---- Untyped quota for the guest's own kernel objects.
    let mut untyped = Vec::new();
    let mut remaining = cfg.untyped_quota;
    while remaining >= PAGE && untyped.len() < abi::MAX_UNTYPED {
        let bits = (usize::BITS - 1 - remaining.leading_zeros()) as usize;
        let bits = bits.min(28);
        let s = alloc.slots.alloc();
        if alloc.pool.split(bits, s).is_none() {
            alloc.slots.free(s);
            break;
        }
        g.arena.slots.push(s);
        g.give(abi::SLOT_UNTYPED_BASE + untyped.len(), root_cap::<cap_type::Untyped>(s), CapRights::all());
        untyped.push(bits);
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
    let dev_cn = g.arena.object_cap::<cap_type::CNode>(&mut alloc.slots, ObjectBlueprint::CNode { size_bits: abi::ROOT_CNODE_BITS });
    g.give_badged(abi::SLOT_DEV_CNODE, dev_cn, CapRights::all(), guard6);

    // ---- Hypercall endpoint.
    g.give_badged(abi::SLOT_HYP_EP, host.hyp_ep, sel4::CapRightsBuilder::none().write(true).grant_reply(true).build(), hyp_badge(id, 0));

    // ---- vCPUs.
    for v in 0..cfg.vcpus {
        let core = (id + v) % host.cores;
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

        let sched_control = host.sched_control(core);
        sched_control
            .sched_control_configure_flags(sc_k, host.timeslice_us, host.timeslice_us, 0, 0, 0)
            .expect("configure SC_K");
        sched_control
            .sched_control_configure_flags(sc_u, host.timeslice_us, host.timeslice_us, 0, 0, 0)
            .expect("configure SC_U");

        ktcb.tcb_configure(g.cnode, CNodeCapData::new(0, abi::ROOT_GUARD_BITS), g.vspace, abi::vcpu_ipcbuf_va(v), ipcbuf)
            .expect("kTCB configure");
        let fault_slot = alloc.slots.alloc();
        abs(root_cap::<cap_type::Null>(fault_slot))
            .mint(&abs(host.hyp_ep), CapRights::all(), hyp_badge(id, v) | HYP_BADGE_FAULT)
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
        g.vcpus.push(Vcpu { ktcb, ntfn, core });
    }

    // ---- Boot info.
    let mut bi: ::alloc::boxed::Box<abi::BootInfo> = unsafe { ::alloc::boxed::Box::new_zeroed().assume_init() };
    bi.magic = abi::BOOTINFO_MAGIC;
    bi.abi_version = abi::ABI_VERSION;
    bi.guest_id = id as u64;
    bi.ram_base_va = ram_base as u64;
    bi.ram_size = ram as u64;
    bi.kernel_end_gpa = kernel_end as u64;
    if !cfg.initrd.is_empty() {
        bi.initrd_gpa = initrd_gpa as u64;
        bi.initrd_size = cfg.initrd.len() as u64;
    }
    bi.nr_vcpus = cfg.vcpus as u64;
    bi.tsc_hz = host.tsc_hz;
    bi.nr_untyped = untyped.len() as u64;
    for (i, b) in untyped.iter().enumerate() {
        bi.untyped[i].size_bits = *b as u64;
    }
    bi.nr_vspaces = cfg.vspaces as u64;
    bi.flags = if cfg.dom0 { abi::BOOT_DOM0 } else { 0 };
    let n = cfg.cmdline.len().min(abi::CMDLINE_SIZE - 1);
    bi.cmdline[..n].copy_from_slice(&cfg.cmdline.as_bytes()[..n]);

    let bi_bytes = unsafe {
        core::slice::from_raw_parts(&*bi as *const abi::BootInfo as *const u8, core::mem::size_of::<abi::BootInfo>())
    };
    for p in 0..(abi::BOOTINFO_SIZE as usize / PAGE) {
        let frame = g.arena.object_cap::<cap_type::Granule>(&mut alloc.slots, FRAME_BP);
        let lo = (p * PAGE).min(bi_bytes.len());
        let hi = ((p + 1) * PAGE).min(bi_bytes.len());
        let chunk = &bi_bytes[lo..hi];
        win.with_frame(alloc, &abs(frame), |dst| unsafe {
            ptr::copy_nonoverlapping(chunk.as_ptr(), dst, chunk.len());
        });
        let vspace = g.vspace;
        map_frame(&mut g.ctx(alloc), vspace, frame, abi::BOOTINFO_VA as usize + p * PAGE, CapRights::read_only(), VmAttributes::DEFAULT);
    }

    // ---- Start vCPU 0 at the kernel entry point.
    let mut regs = sel4::UserContext::default();
    *regs.pc_mut() = elf.entry;
    *regs.c_param_mut(0) = abi::BOOTINFO_VA;
    g.vcpus[0].ktcb.tcb_write_all_registers(true, &mut regs).expect("start vCPU 0");
    g.running = true;

    sel4::debug_println!(
        "l4h-root: guest {} '{}' started: {} MiB RAM in {} chunk(s) at {:#x}, {} vCPU(s), {} MiB untyped",
        id, g.name, ram >> 20, g.ram.len(), ram_base, cfg.vcpus,
        untyped.iter().map(|b| 1usize << b).sum::<usize>() >> 20
    );
    g
}
