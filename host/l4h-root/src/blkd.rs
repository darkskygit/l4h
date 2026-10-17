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

/// MSI vectors handed out so far (seL4 delivers vector n at 0x30 + n).
static mut NEXT_MSI: u64 = 0;

pub fn is_virtio_blk(d: &pci::Device) -> bool {
    d.vendor == VIRTIO_VENDOR
        && (d.device == 0x1042 || d.device == 0x1001)
        && d.virtio_caps.iter().any(|c| c.cfg_type == VIRTIO_PCI_CAP_COMMON_CFG)
        && d.msix.is_some()
}

pub struct Blkd {
    pub comp: Component,
    #[allow(dead_code)]
    pub arena: Arena,
}

fn bdf(d: &pci::Device) -> u32 {
    ((d.bus as u32) << 8) | ((d.dev as u32) << 3) | d.func as u32
}

/// Create a blkd for `d` serving guest device `dev_index` of `client`.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    id: u32,
    d: &pci::Device,
    pci: &Pci,
    client: &mut Guest,
    dev_index: usize,
    devmem: &mut DevMem,
    alloc: &mut Alloc,
    win: &mut Window,
    asids: &mut AsidPools,
    host_info: &HostInfo,
) -> (Blkd, vmm::Backend) {
    let msix = d.msix.unwrap();
    let region = |cfg_type: u8| -> (blk::PciRegion, u32) {
        let c = d
            .virtio_caps
            .iter()
            .find(|c| c.cfg_type == cfg_type)
            .unwrap_or_else(|| panic!("virtio-blk {:x}: no capability {}", bdf(d), cfg_type));
        let va = host::MMIO_VA + c.bar as usize * BAR_STRIDE + c.offset as usize;
        (blk::PciRegion { va: va as u64, len: c.length as u64 }, c.notify_off_multiplier)
    };
    let (common, _) = region(VIRTIO_PCI_CAP_COMMON_CFG);
    let (notify, mult) = region(VIRTIO_PCI_CAP_NOTIFY_CFG);
    let (isr, _) = region(VIRTIO_PCI_CAP_ISR_CFG);
    let (device, _) = region(VIRTIO_PCI_CAP_DEVICE_CFG);
    let msix_table = blk::PciRegion {
        va: (host::MMIO_VA + msix.table_bar as usize * BAR_STRIDE + msix.table_offset as usize) as u64,
        len: msix.table_size as u64 * 16,
    };

    let mut arena = alloc.arena(8 << 20).expect("blkd arena");

    // ---- DMA memory: one physically contiguous chunk.
    let dma_slot = alloc.slots.alloc();
    let dma_region = alloc.pool.split(DMA_BITS, dma_slot).expect("blkd DMA memory");
    let dma_paddr = dma_region.paddr;
    let n_dma = (1 << DMA_BITS) / PAGE;
    let dma_frames = alloc.slots.alloc_range(n_dma);
    UtPool { regions: alloc::vec![dma_region] }
        .retype(&FRAME_BP, n_dma, Dest::root(dma_frames.start))
        .expect("blkd DMA frames");
    arena.slots.push(dma_slot);

    // ---- Channel to the client's l4h-vmm, in the client's arena.
    let chan_pages = core::mem::size_of::<blk::Channel>().div_ceil(PAGE);
    let chan: Vec<cap::Granule> = (0..chan_pages)
        .map(|_| client.arena.object_cap::<cap_type::Granule>(&mut alloc.slots, FRAME_BP))
        .collect();

    let mut cfg: alloc::boxed::Box<blk::BlkdConfig> = unsafe { alloc::boxed::Box::new_zeroed().assume_init() };
    cfg.pci_bdf = bdf(d);
    let vector = unsafe {
        let v = NEXT_MSI;
        NEXT_MSI += 1;
        v
    };
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
    let mut bars: Vec<u8> = d.virtio_caps.iter().map(|c| c.bar).collect();
    bars.push(msix.table_bar);
    bars.sort();
    bars.dedup();
    for b in bars {
        let bar = d.bars[b as usize];
        assert!(!bar.is_io && bar.size > 0 && bar.size as usize <= BAR_STRIDE, "virtio-blk BAR {} unusable", b);
        let frames = devmem
            .frames(&mut alloc.slots, bar.addr as usize, bar.size as usize)
            .unwrap_or_else(|| panic!("no device untyped for BAR {} at {:#x}", b, bar.addr));
        for (i, f) in frames.iter().enumerate() {
            comp.map_copy(
                alloc,
                &mut arena.pool,
                &abs_of(*f),
                host::MMIO_VA + b as usize * BAR_STRIDE + i * PAGE,
                CapRights::read_write(),
                VmAttributes::CACHE_DISABLED,
                keep_slot(),
            );
        }
    }
    for i in 0..n_dma {
        let f = sel4::init_thread::Slot::<cap_type::Granule>::from_index(dma_frames.start + i).cap();
        comp.map_copy(alloc, &mut arena.pool, &abs_of(f), host::DMA_VA + i * PAGE, CapRights::read_write(), VmAttributes::DEFAULT, keep_slot());
    }
    for (i, f) in chan.iter().enumerate() {
        comp.map_copy(alloc, &mut arena.pool, &abs_of(*f), host::blkd_chan_va() + i * PAGE, CapRights::read_write(), VmAttributes::DEFAULT, keep_slot());
    }

    // ---- Interrupt, notifications.
    let irq_slot = alloc.slots.alloc();
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

    let backend = vmm::Backend::Blk { chan, kick };
    (Blkd { comp, arena }, backend)
}
