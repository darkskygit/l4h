//! Networking: the netd component (switches and uplink NICs) and the ports
//! that connect guests' virtio-net devices to it.

use alloc::vec::Vec;

use sel4::{CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

use l4h_abi as abi;
use l4h_host::fixed::ArrayVec;
use l4h_host::{self as host, net};

use crate::blkd::{VirtioRegions, is_legacy_virtio, map_bars, virtio_regions};
use crate::component::{self, Component, SpawnArgs, abs_of};
use crate::devmem::DevMem;
use crate::guest::Guest;
use crate::mem::{Alloc, Arena, Dest};
use crate::pci::{self, Pci};
use crate::vmm;
use crate::vspace::FRAME_BP;
use crate::window::Window;
use crate::{AsidPools, HostInfo, ctl, mr, set_mr};

static NETD_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/l4h-netd.elf"));

const PAGE: usize = 4096;
const NIC_DMA_BITS: usize = 22;
/// Port memory: the channel plus the CNode keeping netd's mappings of it.
const PORT_BITS: usize = 22;
const PORT_KEEP_BITS: usize = 10;
/// MSI vectors of NICs start here.
const NIC_MSI_BASE: u64 = crate::pci::VECTOR_NIC_BASE;
const NETD_ID: u32 = 0x200;

pub fn is_virtio_net(d: &pci::Device) -> bool {
    d.vendor == 0x1af4
        && (d.device == 0x1041 || d.device == 0x1000)
        && (d.virtio_caps.iter().any(|c| c.cfg_type == 1) || is_legacy(d))
        && d.msix.is_some()
}

/// A legacy virtio-net device, as on GCE.
fn is_legacy(d: &pci::Device) -> bool {
    d.device == 0x1000 && is_legacy_virtio(d)
}

/// Google Virtual NIC (gVNIC), as on GCE C3, N4, C4 and later: registers
/// in BAR 0, doorbells in BAR 2.  It needs three MSI-X entries (two
/// notification blocks and the management vector).
pub fn is_gvnic(d: &pci::Device) -> bool {
    let mem = |b: usize| !d.bars[b].is_io && d.bars[b].size != 0;
    d.vendor == 0x1ae0 && d.device == 0x0042 && mem(0) && mem(2) && d.msix.is_some_and(|m| m.table_size >= 3)
}

/// Intel X550 (Linux ixgbe), as on Latitude.sh m4.metal servers:
/// registers in BAR 0, MSI-X with an entry each for RX and TX.
pub fn is_ixgbe(d: &pci::Device) -> bool {
    // X550T, X550T1.
    d.vendor == 0x8086
        && matches!(d.device, 0x1563 | 0x15d1)
        && !d.bars[0].is_io
        && d.bars[0].size != 0
        && d.msix.is_some_and(|m| m.table_size >= 2)
}

/// NICs netd can drive as an uplink.
pub fn is_uplink(d: &pci::Device) -> bool {
    is_virtio_net(d) || is_gvnic(d) || is_ixgbe(d)
}

pub struct Nic {
    pub pci: pci::Device,
    pub switch: Option<u32>,
}

pub struct PortInfo {
    pub vm: usize,
    pub switch: u32,
    pub mac: u64,
}

pub struct Netd {
    comp: Component,
    #[allow(dead_code)]
    arena: Arena,
    pub nics: ArrayVec<Nic, { net::MAX_NICS }>,
    pub ports: [Option<PortInfo>; net::MAX_PORTS],
    /// Switches 0..switches exist (they are never deleted).
    pub switches: u32,
}

pub fn bdf(d: &pci::Device) -> u32 {
    ((d.bus as u32) << 8) | ((d.dev as u32) << 3) | d.func as u32
}

fn call(ep: cap::Endpoint, label: u64, args: &[u64]) -> u64 {
    for (i, a) in args.iter().enumerate() {
        set_mr(i, *a);
    }
    ep.call(sel4::MessageInfo::new(label, 0, 0, args.len()));
    mr(0)
}

/// Start netd with the uplink NICs `nics`.
pub fn spawn(
    nics: Vec<pci::Device>,
    pci: &Pci,
    devmem: &mut DevMem,
    alloc: &mut Alloc,
    win: &mut Window,
    asids: &mut AsidPools,
    info: &HostInfo,
) -> Netd {
    let nr_nics = nics.len().min(net::MAX_NICS);
    let mut arena = alloc.arena(16 << 20).expect("netd arena");
    // Plain integers: all zeros is a valid (empty) configuration.
    let mut cfg: net::NetdConfig = unsafe { core::mem::zeroed() };
    cfg.nr_nics = nr_nics as u32;
    let mut dma = ArrayVec::<_, { net::MAX_NICS }>::new();
    for (n, d) in nics[..nr_nics].iter().enumerate() {
        let bar_va = |b: u8| net::nic_bar_va(n, b as usize);
        let bar_region = |b: u8| l4h_host::blk::PciRegion { va: bar_va(b) as u64, len: d.bars[b as usize].size };
        let none = l4h_host::blk::PciRegion { va: 0, len: 0 };
        let gvnic = is_gvnic(d);
        let ixgbe = is_ixgbe(d);
        let VirtioRegions { common, notify, notify_mult, isr, device, msix_table } = if is_legacy(d) || gvnic || ixgbe {
            let msix = d.msix.unwrap();
            VirtioRegions {
                common: none,
                notify: none,
                notify_mult: 0,
                isr: none,
                device: none,
                msix_table: l4h_host::blk::PciRegion {
                    va: (bar_va(msix.table_bar) + msix.table_offset as usize) as u64,
                    len: msix.table_size as u64 * 16,
                },
            }
        } else {
            virtio_regions(d, bar_va)
        };
        let s = alloc.slots.alloc();
        let mut region = alloc.split(NIC_DMA_BITS, s).expect("NIC DMA memory");
        let paddr = region.paddr;
        let frames = alloc.slots.alloc_range((1 << NIC_DMA_BITS) / PAGE);
        region
            .retype(&FRAME_BP, frames.len(), Dest::root(frames.start))
            .expect("NIC DMA frames");
        let _ = dma.push(frames);
        cfg.nics[n] = net::NicConfig {
            kind: if gvnic {
                net::NIC_GVNIC
            } else if ixgbe {
                net::NIC_IXGBE
            } else {
                net::NIC_VIRTIO
            },
            revision: d.revision as u32,
            pci_bdf: bdf(d),
            msi_data: pci::msi_data(NIC_MSI_BASE + n as u64),
            msi_address: pci::msi_address(),
            common,
            notify,
            notify_off_multiplier: notify_mult,
            legacy_io: if is_legacy(d) { d.bars[0].addr as u32 } else { 0 },
            isr,
            device,
            msix_table,
            regs: if gvnic || ixgbe { bar_region(0) } else { none },
            doorbells: if gvnic { bar_region(2) } else { none },
            dma_paddr: paddr as u64,
            dma_size: 1 << NIC_DMA_BITS,
        };
    }
    let cfg_bytes = unsafe {
        core::slice::from_raw_parts(&cfg as *const net::NetdConfig as *const u8, core::mem::size_of::<net::NetdConfig>())
    };
    let args = SpawnArgs {
        id: NETD_ID,
        kind: host::KIND_NETD,
        elf: NETD_ELF,
        core: 0,
        prio: abi::PRIO_KERNEL + 2,
        budget_us: info.timeslice_us,
        period_us: info.timeslice_us,
        config: cfg_bytes,
        ntfn: None,
        service_ep: None,
        cnode_bits: None,
    };
    let comp = component::create(&args, alloc, &mut arena, win, asids, info);

    // Mapping caps of every NIC page: DMA buffers and BARs.
    const KEEP_BITS: usize = 13;
    const _: () = assert!(net::MAX_NICS * ((1 << NIC_DMA_BITS) / PAGE + 1024) <= 1 << KEEP_BITS);
    let keep = arena.object_cap::<cap_type::CNode>(alloc, ObjectBlueprint::CNode { size_bits: KEEP_BITS });
    let mut keep_next = 0u64;
    let mut keep_slot = || {
        assert!(keep_next < 1 << KEEP_BITS, "netd: too many NIC pages");
        let s = keep.absolute_cptr_from_bits_with_depth(keep_next, KEEP_BITS);
        keep_next += 1;
        s
    };
    let mut nic_list = ArrayVec::new();
    for (n, d) in nics.into_iter().take(nr_nics).enumerate() {
        let mut bars = Vec::new();
        let virtio;
        let used: &[u8] = if is_gvnic(&d) {
            &[0, 2, d.msix.unwrap().table_bar]
        } else if is_ixgbe(&d) {
            &[0, d.msix.unwrap().table_bar]
        } else {
            virtio = crate::blkd::virtio_bars(&d);
            &virtio
        };
        map_bars(&comp, alloc, &mut arena, devmem, &d, &mut bars, used, |b| net::nic_bar_va(n, b as usize), &mut keep_slot);
        for (i, s) in dma[n].clone().enumerate() {
            let f = sel4::init_thread::Slot::<cap_type::Granule>::from_index(s).cap();
            comp.map_copy(alloc, &mut arena, &abs_of(f), net::nic_dma_va(n) + i * PAGE, CapRights::read_write(), VmAttributes::DEFAULT, keep_slot());
        }
        if is_legacy(&d) {
            let (base, len) = (d.bars[0].addr as u16, d.bars[0].size as u16);
            let io = pci::io_ports(alloc, base, len).expect("NIC I/O ports");
            comp.give(net::NETD_SLOT_IOPORT_BASE + n as u64, io, CapRights::all());
        }
        // Interrupt.
        let irq_slot = alloc.slots.alloc();
        pci::msi_irq(
            &d,
            NIC_MSI_BASE + n as u64,
            &abs_of(sel4::init_thread::Slot::<cap_type::Null>::from_index(irq_slot).cap()),
        );
        let irq = sel4::init_thread::Slot::<cap_type::IrqHandler>::from_index(irq_slot).cap();
        let ntfn_slot = alloc.slots.alloc();
        let ntfn = sel4::init_thread::Slot::<cap_type::Notification>::from_index(ntfn_slot).cap();
        abs_of(ntfn)
            .mint(&abs_of(comp.ntfn), CapRights::all(), 1 << (net::NETD_BADGE_IRQ_SHIFT as usize + n))
            .expect("NIC IRQ notification");
        irq.irq_handler_set_notification(ntfn).expect("NIC IRQ set notification");
        comp.give(net::NETD_SLOT_IRQ_BASE + n as u64, irq, CapRights::all());
        pci.enable(&d);
        pci.enable_msix(&d);
        let _ = nic_list.push(Nic { pci: d, switch: None });
    }
    component::start(&comp);
    Netd { comp, arena, nics: nic_list, ports: [const { None }; net::MAX_PORTS], switches: 0 }
}

impl Netd {
    /// A new switch; ENOSPC once there are MAX_SWITCHES, the size of
    /// netd's table of them.
    pub fn try_create_switch(&mut self) -> Result<u32, i64> {
        if self.switches as usize >= net::MAX_SWITCHES {
            return Err(ctl::ENOSPC);
        }
        self.switches += 1;
        Ok(self.switches - 1)
    }

    /// A switch of the boot configuration.
    pub fn create_switch(&mut self) -> u32 {
        self.try_create_switch().expect("l4h-root: more switches than MAX_SWITCHES")
    }

    pub fn attach_uplink(&mut self, switch: u32, nic: usize) -> Result<(), i64> {
        if switch >= self.switches {
            return Err(ctl::ENOENT);
        }
        let n = self.nics.get_mut(nic).ok_or(ctl::ENOENT)?;
        if n.switch.is_some() {
            return Err(ctl::EBUSY);
        }
        n.switch = Some(switch);
        call(self.comp.service_ep, net::NETD_ATTACH_UPLINK, &[nic as u64, switch as u64]);
        Ok(())
    }

    /// Connect guest device `d` of `g` to `switch`.  Returns the port and
    /// the backend for the guest's l4h-vmm.
    pub fn add_port(
        &mut self,
        g: &mut Guest,
        d: usize,
        switch: u32,
        mac: u64,
        lock: bool,
        isolated: bool,
        alloc: &mut Alloc,
    ) -> Result<(usize, vmm::Backend), i64> {
        if switch >= self.switches {
            return Err(ctl::ENOENT);
        }
        let p = self.ports.iter().position(|p| p.is_none()).ok_or(ctl::ENOSPC)?;
        // Port memory comes from the guest: it is reclaimed with it.
        let s = alloc.slots.alloc();
        let region = alloc.split(PORT_BITS, s).ok_or(ctl::ENOMEM)?;
        g.arena.own(s, region.paddr, PORT_BITS);
        let mut pool = region;
        let frames = alloc.slots.alloc_range(net::PORT_PAGES);
        let chan_slots = frames.clone();
        pool.retype(&FRAME_BP, frames.len(), Dest::root(frames.start)).expect("port frames");
        // CNodes keeping netd's and the VMM's mappings of the port.
        let keep_slots = alloc.slots.alloc_range(2);
        pool.retype(&ObjectBlueprint::CNode { size_bits: PORT_KEEP_BITS }, 2, Dest::root(keep_slots.start)).expect("port CNodes");
        let keep = sel4::init_thread::Slot::<cap_type::CNode>::from_index(keep_slots.start).cap();
        let vmm_keep = sel4::init_thread::Slot::<cap_type::CNode>::from_index(keep_slots.start + 1).cap();
        let chan = frames.map(|i| sel4::init_thread::Slot::<cap_type::Granule>::from_index(i).cap());
        for (i, f) in chan.enumerate() {
            let dst = keep.absolute_cptr_from_bits_with_depth(i as u64, PORT_KEEP_BITS);
            // Paging structures for the port come from netd's own pool, and
            // outlive the guest: their root slots must not be tracked into
            // it, or destroying it deletes them (and the arena regions they
            // grew into) while other ports still use them.
            let (comp, arena) = (&self.comp, &mut self.arena);
            alloc.global(|alloc| {
                comp.map_copy(alloc, arena, &abs_of(f), net::port_va(p) + i * PAGE, CapRights::read_write(), VmAttributes::DEFAULT, dst)
            });
        }
        self.comp.give_badged(
            net::NETD_SLOT_CLIENT_BASE + p as u64,
            g.vmm_ntfn,
            CapRights::write_only(),
            1 << (host::VMM_BADGE_DONE_SHIFT as usize + d),
        );
        let kick_slot = alloc.slots.alloc();
        let kick = sel4::init_thread::Slot::<cap_type::Notification>::from_index(kick_slot).cap();
        abs_of(kick).mint(&abs_of(self.comp.ntfn), CapRights::write_only(), 1 << p).expect("port kick");
        let flags = if lock { net::NETD_PORT_MAC_LOCK } else { 0 } | if isolated { net::NETD_PORT_ISOLATED } else { 0 };
        call(self.comp.service_ep, net::NETD_ADD_PORT, &[p as u64, switch as u64, mac, flags]);
        self.ports[p] = Some(PortInfo { vm: g.id, switch, mac });
        let mac_bytes = mac.to_be_bytes();
        Ok((p, vmm::Backend::Net { chan: chan_slots, kick, mac: mac_bytes[2..8].try_into().unwrap(), keep: vmm_keep }))
    }

    /// Disconnect port `p`; netd stops using its memory.
    pub fn remove_port(&mut self, p: usize) {
        if self.ports.get_mut(p).and_then(|i| i.take()).is_some() {
            call(self.comp.service_ep, net::NETD_REMOVE_PORT, &[p as u64]);
            let _ = self.comp.slot(net::NETD_SLOT_CLIENT_BASE + p as u64).delete();
        }
    }

    /// (rx frames, tx frames, drops) of port `p`.
    /// CPU time netd used since the last call, in timer ticks.
    pub fn consumed(&self) -> u64 {
        crate::sched::consumed(self.comp.sc)
    }

    /// The MAC address of host NIC `n`.
    pub fn nic_mac(&self, n: usize) -> Option<u64> {
        let m = call(self.comp.service_ep, net::NETD_NIC_MAC, &[n as u64]);
        (m != u64::MAX).then_some(m)
    }

    pub fn port_stats(&self, p: usize) -> (u64, u64, u64) {
        let rx = call(self.comp.service_ep, net::NETD_PORT_STATS, &[p as u64]);
        (rx, mr(1), mr(2))
    }
}
