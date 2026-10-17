//! Spawning a guest's l4h-vmm and giving it access to the guest: RAM,
//! event pages, device pages and the vCPU notifications.

use alloc::vec::Vec;

use sel4::{CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

use l4h_abi as abi;
use l4h_host as host;

use crate::component::{self, Component, SpawnArgs};
use crate::guest::{Guest, dev_event};
use crate::mem::Alloc;
use crate::window::Window;
use crate::{AsidPools, HostInfo};

static VMM_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/l4h-vmm.elf"));

const PAGE: usize = 4096;
const GIB: usize = 1 << 30;

/// Untyped memory a guest's arena needs for its l4h-vmm.
pub fn arena_bytes(ram: usize, vcpus: usize, devices: usize) -> usize {
    // Mapping caps (32 bytes per frame) and page tables for guest RAM,
    // the VMM image with its heap and stack, and small objects.
    2 * (ram / 128 + ram / 256) + 8 * (1 << 20) + (vcpus + devices * 16) * 4 * PAGE
}

/// A device's host-side backend.
pub enum Backend {
    Blk {
        /// Frames of the blkd channel, mapped at vmm_chan_va(d).
        chan: Vec<cap::Granule>,
        /// Notification of blkd (badged for this client).
        kick: cap::Notification,
    },
}

pub struct Vmm {
    pub comp: Component,
}

const SLOT_KICK_BASE: u64 = 32;

pub fn spawn(
    g: &mut Guest,
    backends: &[Backend],
    alloc: &mut Alloc,
    win: &mut Window,
    asids: &mut AsidPools,
    host_info: &HostInfo,
) -> Vmm {
    assert_eq!(backends.len(), g.dev_pages.len());
    let mut cfg: alloc::boxed::Box<host::VmmConfig> = unsafe { alloc::boxed::Box::new_zeroed().assume_init() };
    cfg.guest_id = g.id as u32;
    cfg.nr_vcpus = g.vcpus.len() as u32;
    cfg.ram_size = g.ram.iter().map(|c| c.size).sum::<usize>() as u64;
    cfg.dom0 = g.dom0 as u32;
    assert!(g.ram.len() <= host::VMM_MAX_RAM_CHUNKS);
    cfg.nr_ram_chunks = g.ram.len() as u32;
    for (i, c) in g.ram.iter().enumerate() {
        cfg.ram_chunks[i] = host::RamChunk { gpa: c.gpa as u64, paddr: c.paddr as u64, size: c.size as u64 };
    }
    assert!(backends.len() <= host::VMM_MAX_DEVICES);
    cfg.nr_devices = backends.len() as u32;
    for (d, b) in backends.iter().enumerate() {
        let dev = &mut cfg.devices[d];
        dev.guest_index = d as u32;
        dev.event = dev_event(d);
        dev.page_va = host::vmm_device_page_va(d) as u64;
        match b {
            Backend::Blk { chan, .. } => {
                dev.kind = host::VDEV_BLK;
                dev.chan_va = host::vmm_chan_va(d) as u64;
                dev.chan_size = (chan.len() * PAGE) as u64;
                dev.chan_kick_slot = SLOT_KICK_BASE + d as u64;
            }
        }
    }
    let cfg_bytes = unsafe {
        core::slice::from_raw_parts(&*cfg as *const host::VmmConfig as *const u8, core::mem::size_of::<host::VmmConfig>())
    };

    let core = g.vcpus[0].core;
    let args = SpawnArgs {
        id: g.id as u32,
        kind: host::KIND_VMM,
        elf: VMM_ELF,
        core,
        prio: abi::PRIO_KERNEL + 1,
        budget_us: host_info.timeslice_us,
        period_us: host_info.timeslice_us,
        config: cfg_bytes,
        ntfn: Some(g.vmm_ntfn),
        service_ep: Some(g.vmm_ep),
    };
    let comp = component::create(&args, alloc, &mut g.arena.pool, win, asids, host_info);

    // ---- Guest RAM at GUEST_RAM_VA + gpa.  The mapping capabilities are
    // kept in CNodes of the guest's arena.
    let ram: usize = cfg.ram_size as usize;
    let mut keep = Vec::new();
    for n in 0..ram.div_ceil(GIB) {
        let frames = (ram - n * GIB).min(GIB) / PAGE;
        let radix = (frames.next_power_of_two().trailing_zeros() as usize).max(1);
        let cn = g.arena.object_cap::<cap_type::CNode>(&mut alloc.slots, ObjectBlueprint::CNode { size_bits: radix });
        keep.push((cn, radix));
    }
    let attrs = VmAttributes::DEFAULT;
    for page in (0..ram).step_by(PAGE) {
        let (cn, radix) = keep[page / GIB];
        let dst = cn.absolute_cptr_from_bits_with_depth(((page % GIB) / PAGE) as u64, radix);
        let frame = g.ram_frame(page);
        comp.map_copy(alloc, &mut g.arena.pool, &frame, host::GUEST_RAM_VA + page, CapRights::read_write(), attrs, dst);
    }

    // ---- Event pages, notifications and device pages.
    let small = g.arena.object_cap::<cap_type::CNode>(&mut alloc.slots, ObjectBlueprint::CNode { size_bits: 10 });
    let mut small_next = 0u64;
    let mut keep_slot = || {
        let s = small.absolute_cptr_from_bits_with_depth(small_next, 10);
        small_next += 1;
        s
    };
    for (v, vcpu) in g.vcpus.iter().enumerate() {
        let ev = crate::component::abs_of(vcpu.event);
        comp.map_copy(alloc, &mut g.arena.pool, &ev, host::vmm_event_page_va(v), CapRights::read_write(), attrs, keep_slot());
        comp.give_badged(host::VCPU_NTFN_SLOT_BASE + v as u64, vcpu.ntfn, CapRights::write_only(), abi::BADGE_EVENT);
    }
    for (d, page) in g.dev_pages.iter().enumerate() {
        let p = crate::component::abs_of(*page);
        comp.map_copy(alloc, &mut g.arena.pool, &p, host::vmm_device_page_va(d), CapRights::read_write(), attrs, keep_slot());
    }
    for (d, b) in backends.iter().enumerate() {
        match b {
            Backend::Blk { chan, kick, .. } => {
                for (i, f) in chan.iter().enumerate() {
                    let fc = crate::component::abs_of(*f);
                    comp.map_copy(alloc, &mut g.arena.pool, &fc, host::vmm_chan_va(d) + i * PAGE, CapRights::read_write(), attrs, keep_slot());
                }
                comp.give(SLOT_KICK_BASE + d as u64, *kick, CapRights::write_only());
            }
        }
    }
    Vmm { comp }
}
