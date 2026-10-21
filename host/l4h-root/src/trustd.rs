//! The attestation service, l4h-trustd (DESIGN.md §9.2): finding the TPM,
//! starting trustd with it, giving guests their capability to it, and the
//! registry page through which trustd learns about guests.

use alloc::vec::Vec;

use sel4::{CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

use l4h_abi as abi;
use l4h_host::trust::{self, GuestRecord, TrustdConfig};
use l4h_host as host;

use crate::component::{self, Component, SpawnArgs, abs_of};
use crate::devmem::DevMem;
use crate::guest::Guest;
use crate::mem::{Alloc, Arena, ObjCtx};
use crate::vspace::map_frame;
use crate::window::Window;
use crate::{AsidPools, HostInfo};

static TRUSTD_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/l4h-trustd.elf"));

const PAGE: usize = 4096;
const TRUSTD_ID: u32 = 0x300;
/// The registry page in the root task.
const REGISTRY_VA: usize = 0x0000_7500_0000_0000;

pub struct Trustd {
    comp: Component,
    #[allow(dead_code)]
    arena: Arena,
    registry: *mut GuestRecord,
    next_instance: u64,
}

/// The TPM, from its ACPI table: its interface, and the frames of its
/// registers (and CRB buffers).
#[cfg(target_arch = "x86_64")]
fn find_tpm(
    bootinfo: &sel4::BootInfoPtr,
    alloc: &mut Alloc,
    devmem: &mut DevMem,
    win: &mut Window,
) -> Option<(TrustdConfig, Vec<cap::Granule>)> {
    use crate::blkd::DEVICE_ATTRS;
    /// Most TPM register and buffer space we map.
    const MAX_MMIO: usize = 0x1_0000;
    /// TPM 2.0 at its standard address (PC Client PTP), where the ACPI
    /// table gives none (FIFO interface).
    const TPM_DEFAULT_BASE: usize = 0xfed4_0000;
    let mut phys = crate::acpi::Phys::new(alloc, devmem, win);
    let Some(t) = crate::acpi::find(bootinfo, &mut phys, b"TPM2").filter(|t| t.len() >= 52) else {
        info!("l4h-root: no TPM, no attestation");
        return None;
    };
    let control = u64::from_le_bytes(t[40..48].try_into().unwrap()) as usize;
    let start = u32::from_le_bytes(t[48..52].try_into().unwrap());
    let cfg = |iface, base: usize, size: usize| TrustdConfig {
        iface,
        reserved: 0,
        mmio_paddr: base as u64,
        mmio_size: size as u64,
        cmd_off: 0,
        cmd_size: 0,
        rsp_off: 0,
        rsp_size: 0,
    };
    let cfg = match start {
        // FIFO ("TIS"): locality 0's registers.
        6 => Some(cfg(trust::TPM_TIS, if control != 0 { control & !(PAGE - 1) } else { TPM_DEFAULT_BASE }, PAGE)),
        // CRB with the start register: the control area is at 0x40 in
        // locality 0's page; the buffers are where it says.
        7 if control % PAGE == 0x40 => {
            let regs = phys.read_attrs(control, 0x30, DEVICE_ATTRS)?;
            let w32 = |i: usize| u32::from_le_bytes(regs[i..i + 4].try_into().unwrap()) as usize;
            let cmd = w32(0x1c) | w32(0x20) << 32;
            let (cmd_size, rsp_size) = (w32(0x18), w32(0x24));
            let rsp = w32(0x28) | w32(0x2c) << 32;
            let base = (control & !(PAGE - 1)).min(cmd).min(rsp) & !(PAGE - 1);
            let end = (control + 0x30).max(cmd + cmd_size).max(rsp + rsp_size).next_multiple_of(PAGE);
            if end - base > MAX_MMIO {
                sel4::debug_println!("l4h-root: TPM CRB buffers too far from its registers, no attestation");
                return None;
            }
            Some(TrustdConfig {
                cmd_off: (cmd - base) as u64,
                cmd_size: cmd_size as u64,
                rsp_off: (rsp - base) as u64,
                rsp_size: rsp_size as u64,
                ..cfg(trust::TPM_CRB, base, end - base)
            })
        }
        _ => {
            sel4::debug_println!("l4h-root: TPM start method {} unsupported, no attestation", start);
            None
        }
    }?;
    let mmio: Option<Vec<_>> = (0..cfg.mmio_size as usize / PAGE).map(|p| phys.frame(cfg.mmio_paddr as usize + p * PAGE)).collect();
    if mmio.is_none() {
        sel4::debug_println!("l4h-root: TPM registers at {:#x} are not device memory, no attestation", cfg.mmio_paddr);
    }
    Some((cfg, mmio?))
}

#[cfg(not(target_arch = "x86_64"))]
fn find_tpm(
    _bootinfo: &sel4::BootInfoPtr,
    _alloc: &mut Alloc,
    _devmem: &mut DevMem,
    _win: &mut Window,
) -> Option<(TrustdConfig, Vec<cap::Granule>)> {
    None
}

/// Start trustd if the host has a TPM (and root was built with trustd).
pub fn spawn(
    bootinfo: &sel4::BootInfoPtr,
    alloc: &mut Alloc,
    devmem: &mut DevMem,
    win: &mut Window,
    asids: &mut AsidPools,
    info: &HostInfo,
) -> Option<Trustd> {
    if TRUSTD_ELF.is_empty() {
        return None;
    }
    let (cfg, mmio) = find_tpm(bootinfo, alloc, devmem, win)?;
    info!(
        "l4h-root: TPM 2.0 ({}) at {:#x}",
        if cfg.iface == trust::TPM_CRB { "CRB" } else { "FIFO" },
        cfg.mmio_paddr
    );

    let registry = alloc.object_cap::<cap_type::Granule>(crate::vspace::FRAME_BP);
    alloc.global(|alloc| {
        let copy = alloc.slots.alloc();
        abs_of(sel4::init_thread::Slot::<cap_type::Null>::from_index(copy).cap())
            .copy(&abs_of(registry), CapRights::all())
            .expect("registry copy");
        let f = sel4::init_thread::Slot::<cap_type::Granule>::from_index(copy).cap();
        map_frame(&mut ObjCtx::global(alloc), sel4::init_thread::slot::VSPACE.cap(), f, REGISTRY_VA, CapRights::read_write(), VmAttributes::DEFAULT);
    });

    let mut arena = alloc.arena(8 << 20).expect("trustd arena");
    let cfg_bytes = unsafe {
        core::slice::from_raw_parts(&cfg as *const TrustdConfig as *const u8, core::mem::size_of::<TrustdConfig>())
    };
    let args = SpawnArgs {
        id: TRUSTD_ID,
        kind: host::KIND_TRUSTD,
        elf: TRUSTD_ELF,
        core: info.cores - 1,
        prio: abi::PRIO_KERNEL + 1,
        // At most a quarter of a core: it polls the TPM at start.
        budget_us: info.timeslice_us,
        period_us: 4 * info.timeslice_us,
        config: cfg_bytes,
        ntfn: None,
        service_ep: None,
    };
    let comp = component::create(&args, alloc, &mut arena, win, asids, info);
    let keep = arena.object_cap::<cap_type::CNode>(alloc, ObjectBlueprint::CNode { size_bits: 5 });
    let keep_slot = |i: usize| keep.absolute_cptr_from_bits_with_depth(i as u64, 5);
    for (i, f) in mmio.iter().enumerate() {
        comp.map_copy(alloc, &mut arena, &abs_of(*f), trust::TPM_VA + i * PAGE, CapRights::read_write(), crate::blkd::DEVICE_ATTRS, keep_slot(i));
    }
    comp.map_copy(alloc, &mut arena, &abs_of(registry), trust::REGISTRY_VA, CapRights::read_only(), VmAttributes::DEFAULT, keep_slot(31));
    component::start(&comp);
    Some(Trustd { comp, arena, registry: REGISTRY_VA as *mut GuestRecord, next_instance: 1 })
}

impl Trustd {
    /// Give guest `g` (being created) its capability to trustd.
    pub fn give(&self, g: &mut Guest) {
        g.give_badged(
            abi::SLOT_ATTEST_EP,
            self.comp.service_ep,
            sel4::CapRightsBuilder::none().write(true).grant_reply(true).build(),
            trust::BADGE_GUEST | g.id as u64,
        );
        g.attest = true;
    }

    /// Guest `g` starts: its reports describe it from now on.
    pub fn register(&mut self, g: &Guest) {
        let Some(launch) = g.launch else { return };
        let r = GuestRecord {
            instance: 0,
            ram: g.ram_size() as u64,
            vcpus: g.vcpus.len() as u32,
            flags: if g.dom0 { trust::RECORD_DOM0 } else { 0 },
            launch,
            reserved: 0,
        };
        let p = self.registry.wrapping_add(g.id);
        unsafe {
            p.write_volatile(r);
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            core::ptr::addr_of_mut!((*p).instance).write_volatile(self.next_instance);
        }
        self.next_instance += 1;
    }

    /// Guest `id` is destroyed.
    pub fn unregister(&mut self, id: usize) {
        unsafe { core::ptr::addr_of_mut!((*self.registry.wrapping_add(id)).instance).write_volatile(0) };
    }
}
