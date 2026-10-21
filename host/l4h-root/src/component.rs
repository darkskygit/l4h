//! Spawning host components (l4h-vmm, blkd, netd, trustd) as separate seL4
//! processes with the layout defined in `l4h-host`.

use alloc::collections::BTreeMap;
use core::ptr;

use sel4::init_thread::slot;
use sel4::{CNodeCapData, CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

use l4h_host as host;

use crate::elf::Elf;
use crate::mem::{Alloc, Arena, ObjCtx, Slot};
use crate::vspace::{FRAME_BP, PML4_BP, map_frame};
use crate::window::Window;
use crate::{AsidPools, HostInfo};

const PAGE: usize = 4096;

/// Badge bits on the root endpoint for messages from components.
pub const ROOT_BADGE_COMPONENT: u64 = 1 << 25;

pub fn component_badge(id: u32) -> u64 {
    ROOT_BADGE_COMPONENT | id as u64
}

fn abs(c: impl sel4::HasCPtrWithDepth) -> sel4::AbsoluteCPtr {
    slot::CNODE.cap().absolute_cptr(c)
}

/// A root-CSpace capability as an AbsoluteCPtr.
pub fn abs_of(c: impl sel4::HasCPtrWithDepth) -> sel4::AbsoluteCPtr {
    abs(c)
}

fn root_cap<T: sel4::CapType>(s: Slot) -> sel4::Cap<T> {
    sel4::init_thread::Slot::<T>::from_index(s).cap()
}

pub struct Component {
    pub id: u32,
    pub tcb: cap::Tcb,
    pub cnode: cap::CNode,
    pub vspace: cap::VSpace,
    pub ntfn: cap::Notification,
    pub service_ep: cap::Endpoint,
    pub sc: cap::SchedContext,
}

impl Component {
    /// Destination for a capability in the component's CNode.
    pub fn slot(&self, idx: u64) -> sel4::AbsoluteCPtr {
        self.cnode.absolute_cptr_from_bits_with_depth(idx, host::CNODE_BITS)
    }

    pub fn give(&self, idx: u64, src: impl sel4::HasCPtrWithDepth, rights: CapRights) {
        self.slot(idx)
            .copy(&abs(src), rights)
            .unwrap_or_else(|e| panic!("component {} slot {} copy: {:?}", self.id, idx, e));
    }

    pub fn give_badged(&self, idx: u64, src: impl sel4::HasCPtrWithDepth, rights: CapRights, badge: u64) {
        self.slot(idx)
            .mint(&abs(src), rights, badge)
            .unwrap_or_else(|e| panic!("component {} slot {} mint: {:?}", self.id, idx, e));
    }

    /// Map `frame` (a capability anywhere in the root CSpace) into the
    /// component at `va`.  The mapping capability is a fresh copy that is
    /// moved into `keep` (a CNode slot that must outlive the mapping).
    pub fn map_copy(
        &self,
        alloc: &mut Alloc,
        arena: &mut Arena,
        frame: &sel4::AbsoluteCPtr,
        va: usize,
        rights: CapRights,
        attrs: VmAttributes,
        keep: sel4::AbsoluteCPtr,
    ) {
        let s = alloc.slots.alloc();
        abs(root_cap::<cap_type::Null>(s)).copy(frame, CapRights::all()).expect("copy frame");
        let f = root_cap::<cap_type::Granule>(s);
        map_frame(&mut ObjCtx::arena(alloc, arena), self.vspace, f, va, rights, attrs);
        keep.move_(&abs(f)).expect("keep mapping cap");
        alloc.slots.free(s);
    }
}

pub struct SpawnArgs<'a> {
    pub id: u32,
    pub kind: u32,
    pub elf: &'a [u8],
    pub core: usize,
    pub prio: u64,
    pub budget_us: u64,
    pub period_us: u64,
    /// Kind-specific configuration written to the info page.
    pub config: &'a [u8],
    /// Pre-made bound notification / service endpoint (created if None).
    pub ntfn: Option<cap::Notification>,
    pub service_ep: Option<cap::Endpoint>,
}

/// Create a component, without starting it.  Objects come from `arena`.
pub fn create(
    args: &SpawnArgs,
    alloc: &mut Alloc,
    arena: &mut Arena,
    win: &mut Window,
    asids: &mut AsidPools,
    host_info: &HostInfo,
) -> Component {
    let elf = Elf::parse(args.elf).expect("component ELF");
    let mut ctx = ObjCtx::arena(alloc, arena);


    let vspace = ctx.cap::<cap_type::VSpace>(PML4_BP);
    let cnode = ctx.cap::<cap_type::CNode>(ObjectBlueprint::CNode { size_bits: host::CNODE_BITS });
    let tcb = ctx.cap::<cap_type::Tcb>(ObjectBlueprint::Tcb);
    let ntfn = args.ntfn.unwrap_or_else(|| ctx.cap::<cap_type::Notification>(ObjectBlueprint::Notification));
    let service_ep = args.service_ep.unwrap_or_else(|| ctx.cap::<cap_type::Endpoint>(ObjectBlueprint::Endpoint));
    let reply = ctx.cap::<cap_type::Reply>(ObjectBlueprint::Reply);
    let sc = ctx.cap::<cap_type::SchedContext>(ObjectBlueprint::SchedContext {
        size_bits: sel4::sys::seL4_MinSchedContextBits as usize,
    });
    let ipcbuf = ctx.cap::<cap_type::Granule>(FRAME_BP);
    drop(ctx);
    asids.assign(alloc, vspace);

    let c = Component { id: args.id, tcb, cnode, vspace, ntfn, service_ep, sc };

    // ---- Load the ELF image, one fresh frame per page; pages are writable
    // only if a writable segment covers them.
    let mut writable: BTreeMap<usize, bool> = BTreeMap::new();
    for seg in elf.segments() {
        let start = (seg.vaddr as usize) & !(PAGE - 1);
        for page in (start..(seg.vaddr + seg.memsz) as usize).step_by(PAGE) {
            *writable.entry(page).or_default() |= seg.writable;
        }
    }
    let mut pages: BTreeMap<usize, cap::Granule> = BTreeMap::new();
    for seg in elf.segments() {
        let start = (seg.vaddr as usize) & !(PAGE - 1);
        let end = (seg.vaddr + seg.memsz) as usize;
        for page in (start..end).step_by(PAGE) {
            let frame = *pages.entry(page).or_insert_with(|| {
                let mut ctx = ObjCtx::arena(alloc, &mut *arena);
                let f = ctx.cap::<cap_type::Granule>(FRAME_BP);
                let rights = if writable[&page] { CapRights::read_write() } else { CapRights::read_only() };
                map_frame(&mut ctx, vspace, f, page, rights, VmAttributes::DEFAULT);
                f
            });
            // Copy the part of the segment's file data that lies in this page.
            let seg_lo = seg.vaddr as usize;
            let lo = page.max(seg_lo);
            let hi = (page + PAGE).min(seg_lo + seg.data.len());
            if lo < hi {
                let src = &seg.data[lo - seg_lo..hi - seg_lo];
                // A mapped frame cannot be mapped again: copy the cap for the window.
                win.with_frame(&abs(frame), |p| unsafe {
                    ptr::copy_nonoverlapping(src.as_ptr(), p.add(lo - page), src.len());
                });
            }
        }
    }

    // ---- Info pages and IPC buffer.
    let mut info = alloc::vec![0u8; host::INFO_SIZE];
    let hdr = host::InfoHeader {
        magic: host::INFO_MAGIC,
        kind: args.kind,
        id: args.id,
        tsc_hz: host_info.tsc_hz,
        reserved: [0; 5],
    };
    unsafe {
        ptr::copy_nonoverlapping(
            &hdr as *const host::InfoHeader as *const u8,
            info.as_mut_ptr(),
            core::mem::size_of::<host::InfoHeader>(),
        );
    }
    assert!(host::CONFIG_OFFSET + args.config.len() <= host::INFO_SIZE, "component config too large");
    info[host::CONFIG_OFFSET..host::CONFIG_OFFSET + args.config.len()].copy_from_slice(args.config);
    for (i, chunk) in info.chunks(PAGE).enumerate() {
        let mut ctx = ObjCtx::arena(alloc, &mut *arena);
        let f = ctx.cap::<cap_type::Granule>(FRAME_BP);
        map_frame(&mut ctx, vspace, f, host::INFO_VA + i * PAGE, CapRights::read_only(), VmAttributes::DEFAULT);
        win.with_frame(&abs(f), |p| unsafe {
            ptr::copy_nonoverlapping(chunk.as_ptr(), p, chunk.len());
        });
    }
    map_frame(
        &mut ObjCtx::arena(alloc, &mut *arena),
        vspace,
        ipcbuf,
        host::IPCBUF_VA,
        CapRights::read_write(),
        VmAttributes::DEFAULT,
    );

    // ---- CSpace.
    let guard = CNodeCapData::new(0, 64 - host::CNODE_BITS);
    c.give_badged(host::SLOT_CNODE, cnode, CapRights::all(), guard.into_word());
    c.give(host::SLOT_VSPACE, vspace, CapRights::all());
    c.give(host::SLOT_TCB, tcb, CapRights::all());
    c.give_badged(
        host::SLOT_ROOT_EP,
        host_info.hyp_ep,
        sel4::CapRightsBuilder::none().write(true).grant_reply(true).build(),
        component_badge(args.id),
    );
    c.give(host::SLOT_NTFN, ntfn, CapRights::all());
    c.give(host::SLOT_SERVICE_EP, service_ep, CapRights::all());
    c.give(host::SLOT_REPLY, reply, CapRights::all());

    // ---- Thread.
    tcb.tcb_configure(cnode, CNodeCapData::new(0, 64 - host::CNODE_BITS), vspace, host::IPCBUF_VA as u64, ipcbuf)
        .expect("component TCB configure");
    host_info
        .sched_control(args.core)
        .sched_control_configure_flags(sc, args.budget_us, args.period_us, 0, 0, 0)
        .expect("component SC");
    let fault_slot = alloc.slots.alloc();
    abs(root_cap::<cap_type::Null>(fault_slot))
        .mint(&abs(host_info.hyp_ep), CapRights::all(), component_badge(args.id) | crate::guest::HYP_BADGE_FAULT)
        .expect("component fault ep");
    tcb.tcb_set_sched_params(slot::TCB.cap(), args.prio, args.prio, sc, root_cap::<cap_type::Endpoint>(fault_slot))
        .expect("component sched params");
    tcb.tcb_bind_notification(ntfn).expect("component bind notification");
    let mut regs = sel4::UserContext::default();
    *regs.pc_mut() = elf.entry;
    tcb.tcb_write_all_registers(false, &mut regs).expect("component registers");
    c
}

pub fn start(c: &Component) {
    c.tcb.tcb_resume().expect("start component");
}
