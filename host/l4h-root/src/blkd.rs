//! Passing a virtio-blk PCI device through to a guest: a blkd component
//! drives the device, and the guest's l4h-vmm presents it as virtio-l4h.

use alloc::vec::Vec;

use sel4::init_thread::slot;
use sel4::{CapRights, VmAttributes, cap, cap_type};

use l4h_abi as abi;
use l4h_host::{self as host, blk};

use crate::component::{self, Component, SpawnArgs, abs_of};
use crate::devmem::DevMem;
use crate::guest::Guest;
use crate::mem::{Alloc, Arena, Dest, UtPool};
use crate::pci::{self, Pci};
use crate::vmm;
use crate::vspace::FRAME_BP;
use crate::window::Window;
use crate::{AsidPools, HostInfo};

static BLKD_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/l4h-blkd.elf"));

const PAGE: usize = 4096;
const DMA_BITS: usize = 18;
/// Each BAR is mapped at MMIO_VA + bar * BAR_STRIDE.
const BAR_STRIDE: usize = 0x100_0000;

const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// A virtio-blk PCI device that can be passed through to a guest.
pub struct Disk {
    pub pci: pci::Device,
    /// MSI vector (seL4 delivers vector n at CPU vector 0x30 + n).
    vector: u64,
    /// Frames of the BARs the driver uses, carved once.
    bars: Vec<(u8, Vec<cap::Granule>)>,
    pub owner: Option<usize>,
    pub read_only: bool,
    driver: Option<Blkd>,
}

impl Disk {
    pub fn new(pci: pci::Device, vector: u64) -> Self {
        Disk { pci, vector, bars: Vec::new(), owner: None, read_only: false, driver: None }
    }
}

pub fn is_virtio_blk(d: &pci::Device) -> bool {
    d.vendor == VIRTIO_VENDOR
        && (d.device == 0x1042 || d.device == 0x1001)
        && d.virtio_caps.iter().any(|c| c.cfg_type == VIRTIO_PCI_CAP_COMMON_CFG)
        && d.msix.is_some()
}

struct Blkd {
    comp: Component,
    arena: Arena,
    irq_slot: usize,
}

fn bdf(d: &pci::Device) -> u32 {
    ((d.bus as u32) << 8) | ((d.dev as u32) << 3) | d.func as u32
}

/// Start a blkd for `disk` serving guest device `dev_index` of `client`,
/// and return the backend for the client's l4h-vmm.
#[allow(clippy::too_many_arguments)]
pub fn attach(
    id: u32,
    disk: &mut Disk,
    pci: &Pci,
    client: &mut Guest,
    dev_index: usize,
    devmem: &mut DevMem,
    alloc: &mut Alloc,
    win: &mut Window,
    asids: &mut AsidPools,
    host_info: &HostInfo,
) -> vmm::Backend {
    assert!(disk.driver.is_none());
    let d = &disk.pci;
    let bar_va = |b: u8| host::MMIO_VA + b as usize * BAR_STRIDE;
    let VirtioRegions { common, notify, notify_mult: mult, isr, device, msix_table } = virtio_regions(d, bar_va);

    let mut arena = alloc.arena(8 << 20).expect("blkd arena");

    // ---- DMA memory: one physically contiguous chunk.
    let dma_slot = alloc.slots.alloc();
    let dma_region = alloc.split(DMA_BITS, dma_slot).expect("blkd DMA memory");
    let dma_paddr = dma_region.paddr;
    let n_dma = (1 << DMA_BITS) / PAGE;
    let dma_frames = alloc.slots.alloc_range(n_dma);
    UtPool { regions: alloc::vec![dma_region] }
        .retype(&FRAME_BP, n_dma, Dest::root(dma_frames.start))
        .expect("blkd DMA frames");
    arena.own(dma_slot, dma_paddr, DMA_BITS);

    // ---- Channel to the client's l4h-vmm, in the client's arena.
    let chan_pages = core::mem::size_of::<blk::Channel>().div_ceil(PAGE);
    let chan: Vec<cap::Granule> = (0..chan_pages)
        .map(|_| client.arena.object_cap::<cap_type::Granule>(&mut alloc.slots, FRAME_BP))
        .collect();

    let mut cfg: alloc::boxed::Box<blk::BlkdConfig> = unsafe { alloc::boxed::Box::new_zeroed().assume_init() };
    cfg.pci_bdf = bdf(d);
    let vector = disk.vector;
    cfg.msi_data = 0x30 + vector as u32;
    cfg.common = common;
    cfg.notify = notify;
    cfg.notify_off_multiplier = mult;
    cfg.isr = isr;
    cfg.device = device;
    cfg.msix_table = msix_table;
    cfg.dma_va = host::DMA_VA as u64;
    cfg.dma_paddr = dma_paddr as u64;
    cfg.dma_size = 1 << DMA_BITS;
    cfg.chan_va = host::blkd_chan_va() as u64;
    assert!(client.ram.len() <= blk::BLKD_MAX_RAM_CHUNKS);
    cfg.nr_ram_chunks = client.ram.len() as u32;
    for (i, c) in client.ram.iter().enumerate() {
        cfg.ram_chunks[i] = host::RamChunk { gpa: c.gpa as u64, paddr: c.paddr as u64, size: c.size as u64 };
    }
    let cfg_bytes = unsafe {
        core::slice::from_raw_parts(&*cfg as *const blk::BlkdConfig as *const u8, core::mem::size_of::<blk::BlkdConfig>())
    };

    let args = SpawnArgs {
        id,
        kind: host::KIND_BLKD,
        elf: BLKD_ELF,
        core: 0,
        prio: abi::PRIO_KERNEL + 2,
        budget_us: host_info.timeslice_us,
        period_us: host_info.timeslice_us,
        config: cfg_bytes,
        ntfn: None,
        service_ep: None,
    };
    let comp = component::create(&args, alloc, &mut arena.pool, win, asids, host_info);

    // Mapping caps are kept in a CNode of the arena.
    let keep = arena.object_cap::<cap_type::CNode>(&mut alloc.slots, sel4::ObjectBlueprint::CNode { size_bits: 10 });
    let mut keep_next = 0u64;
    let mut keep_slot = || {
        let s = keep.absolute_cptr_from_bits_with_depth(keep_next, 10);
        keep_next += 1;
        s
    };

    // ---- Device BARs (uncached).
    map_bars(&comp, alloc, &mut arena.pool, devmem, d, &mut disk.bars, bar_va, &mut keep_slot);
    for i in 0..n_dma {
        let f = sel4::init_thread::Slot::<cap_type::Granule>::from_index(dma_frames.start + i).cap();
        comp.map_copy(alloc, &mut arena.pool, &abs_of(f), host::DMA_VA + i * PAGE, CapRights::read_write(), VmAttributes::DEFAULT, keep_slot());
    }
    for (i, f) in chan.iter().enumerate() {
        comp.map_copy(alloc, &mut arena.pool, &abs_of(*f), host::blkd_chan_va() + i * PAGE, CapRights::read_write(), VmAttributes::DEFAULT, keep_slot());
    }

    // ---- Interrupt, notifications.
    let irq_slot = alloc.slots.global(|s| s.alloc());
    slot::IRQ_CONTROL
        .cap()
        .irq_control_get_msi(d.bus as u64, d.dev as u64, d.func as u64, 0, vector, &abs_of(sel4::init_thread::Slot::<cap_type::Null>::from_index(irq_slot).cap()))
        .expect("MSI IRQ handler");
    let irq = sel4::init_thread::Slot::<cap_type::IrqHandler>::from_index(irq_slot).cap();
    let irq_ntfn_slot = alloc.slots.alloc();
    let irq_ntfn = sel4::init_thread::Slot::<cap_type::Notification>::from_index(irq_ntfn_slot).cap();
    abs_of(irq_ntfn).mint(&abs_of(comp.ntfn), CapRights::all(), blk::BLKD_BADGE_IRQ).expect("IRQ notification");
    irq.irq_handler_set_notification(irq_ntfn).expect("IRQ set notification");
    comp.give(blk::BLKD_SLOT_IRQ, irq, CapRights::all());
    comp.give_badged(
        blk::BLKD_SLOT_CLIENT_DONE,
        client.vmm_ntfn,
        CapRights::write_only(),
        1 << (host::VMM_BADGE_DONE_SHIFT as usize + dev_index),
    );
    let kick_slot = alloc.slots.alloc();
    let kick = sel4::init_thread::Slot::<cap_type::Notification>::from_index(kick_slot).cap();
    abs_of(kick).mint(&abs_of(comp.ntfn), CapRights::write_only(), blk::BLKD_BADGE_KICK).expect("blkd kick");

    pci.enable(d);
    pci.enable_msix(d);
    component::start(&comp);

    disk.owner = Some(client.id);
    disk.driver = Some(Blkd { comp, arena, irq_slot });
    vmm::Backend::Blk { chan, kick }
}

/// Stop the driver of `disk` and free it.  Device DMA is stopped first, so
/// that the client's memory can be reused afterwards.
pub fn detach(disk: &mut Disk, pci: &Pci, alloc: &mut Alloc) {
    pci.disable(&disk.pci);
    if let Some(b) = disk.driver.take() {
        let _ = b.comp.tcb.tcb_suspend();
        let irq = sel4::init_thread::Slot::<cap_type::IrqHandler>::from_index(b.irq_slot).cap();
        let _ = irq.irq_handler_clear();
        abs_of(irq).delete().expect("delete IRQ handler");
        alloc.slots.free(b.irq_slot);
        alloc.reclaim(b.arena);
    }
    disk.owner = None;
}

/// The virtio-pci structures of a device, at the addresses its BARs are
/// mapped at (`bar_va`).
pub struct VirtioRegions {
    pub common: blk::PciRegion,
    pub notify: blk::PciRegion,
    pub notify_mult: u32,
    pub isr: blk::PciRegion,
    pub device: blk::PciRegion,
    pub msix_table: blk::PciRegion,
}

pub fn virtio_regions(d: &pci::Device, bar_va: impl Fn(u8) -> usize) -> VirtioRegions {
    let region = |cfg_type: u8| -> (blk::PciRegion, u32) {
        let c = d
            .virtio_caps
            .iter()
            .find(|c| c.cfg_type == cfg_type)
            .unwrap_or_else(|| panic!("virtio device {:x}: no capability {}", bdf(d), cfg_type));
        let va = bar_va(c.bar) + c.offset as usize;
        (blk::PciRegion { va: va as u64, len: c.length as u64 }, c.notify_off_multiplier)
    };
    let msix = d.msix.unwrap();
    VirtioRegions {
        common: region(VIRTIO_PCI_CAP_COMMON_CFG).0,
        notify: region(VIRTIO_PCI_CAP_NOTIFY_CFG).0,
        notify_mult: region(VIRTIO_PCI_CAP_NOTIFY_CFG).1,
        isr: region(VIRTIO_PCI_CAP_ISR_CFG).0,
        device: region(VIRTIO_PCI_CAP_DEVICE_CFG).0,
        msix_table: blk::PciRegion {
            va: (bar_va(msix.table_bar) + msix.table_offset as usize) as u64,
            len: msix.table_size as u64 * 16,
        },
    }
}

/// Map the BARs holding `d`'s virtio structures and MSI-X table into
/// `comp`, uncached.  BAR frames are carved once and cached in `cache`.
#[allow(clippy::too_many_arguments)]
pub fn map_bars(
    comp: &Component,
    alloc: &mut Alloc,
    pool: &mut UtPool,
    devmem: &mut DevMem,
    d: &pci::Device,
    cache: &mut Vec<(u8, Vec<cap::Granule>)>,
    bar_va: impl Fn(u8) -> usize,
    keep: &mut impl FnMut() -> sel4::AbsoluteCPtr,
) {
    // The common, notify, ISR and device structures (types 1-4); others
    // (PCI config access, shared memory) are not used.
    let mut bars: Vec<u8> = d.virtio_caps.iter().filter(|c| (1..=4).contains(&c.cfg_type)).map(|c| c.bar).collect();
    bars.extend(d.msix.map(|m| m.table_bar));
    bars.sort();
    bars.dedup();
    for b in bars {
        if !cache.iter().any(|(i, _)| *i == b) {
            let bar = d.bars[b as usize];
            assert!(!bar.is_io && bar.size > 0 && bar.size as usize <= BAR_STRIDE, "virtio BAR {} unusable", b);
            let frames = alloc
                .slots
                .global(|slots| devmem.frames(slots, bar.addr as usize, bar.size as usize))
                .unwrap_or_else(|| panic!("no device untyped for BAR {} at {:#x}", b, bar.addr));
            cache.push((b, frames));
        }
        let frames = &cache.iter().find(|(i, _)| *i == b).unwrap().1;
        for (i, f) in frames.iter().enumerate() {
            comp.map_copy(
                alloc,
                pool,
                &abs_of(*f),
                bar_va(b) + i * PAGE,
                CapRights::read_write(),
                VmAttributes::CACHE_DISABLED,
                keep(),
            );
        }
    }
}
