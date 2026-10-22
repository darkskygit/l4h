//! The attestation service, l4h-trustd (DESIGN.md §9.2): finding the TPM,
//! starting trustd with it, giving guests their attestation device, the
//! registry page through which trustd learns about guests, and what
//! trustd asks root for: raising a device's event when it has answered,
//! and the hardware accesses of its AML interpreter.
//!
//! Only x86 finds a TPM (in ACPI) so far: on aarch64, trustd never starts.
#![cfg_attr(not(target_arch = "x86_64"), allow(dead_code, unused_imports))]


use sel4::{CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

use l4h_abi as abi;
use l4h_host as host;
use l4h_host::fixed::ArrayVec;
use l4h_host::trust::{self, GuestRecord, TrustdConfig};

use crate::component::{self, Component, SpawnArgs, abs_of};
use crate::devmem::DevMem;
use crate::guest::Guest;
use crate::mem::{Alloc, Arena, ObjCtx};
use crate::vspace::{FRAME_BP, map_frame};
use crate::window::Window;
use crate::{AsidPools, Host, HostInfo, mr};

pub use l4h_host::trust::AML_MAX;

static TRUSTD_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/l4h-trustd.elf"));

const PAGE: usize = 4096;
pub const TRUSTD_ID: u32 = 0x300;
/// The registry page in the root task.
const REGISTRY_VA: usize = crate::layout::TRUSTD_REGISTRY.base;
/// Mapping capabilities trustd's ranges and AML pages can need.
const KEEP_BITS: usize = 12;

pub struct Trustd {
    comp: Component,
    #[allow(dead_code)]
    arena: Arena,
    registry: *mut GuestRecord,
    next_instance: u64,
    /// The request page of each guest id, mapped in trustd.
    pages: [cap::Granule; trust::MAX_RECORDS],
    /// Guests signal it, badged with trust::kick_badge(id).
    kick: cap::Notification,
    /// Device memory carved for trustd's ranges and its AML's accesses.
    #[cfg(target_arch = "x86_64")]
    frames: crate::acpi::Frames,
}

/// Where the TPM is: its interface and the ranges trustd maps, with the
/// registers and buffers in them as (range, offset in it).
#[derive(Default)]
struct Layout {
    iface: u32,
    start: u32,
    ranges: ArrayVec<(usize, usize), { trust::MAX_RANGES }>,
    loc: Option<(usize, usize)>,
    ctrl: Option<(usize, usize)>,
    cmd: Option<(usize, usize, usize)>,
    rsp: Option<(usize, usize, usize)>,
    start_bell: Option<(usize, usize)>,
    reply_bell: Option<(usize, usize)>,
}

impl Layout {
    /// A new range covering [paddr, paddr + len): (range, offset).
    fn range(&mut self, paddr: usize, len: usize) -> Option<(usize, usize)> {
        let base = paddr & !(PAGE - 1);
        let size = (paddr + len).next_multiple_of(PAGE) - base;
        if size > trust::RANGE_STRIDE {
            return None;
        }
        self.ranges.push((base, size)).ok()?;
        Some((self.ranges.len() - 1, paddr - base))
    }

    fn va((r, off): (usize, usize)) -> u64 {
        (trust::TPM_VA + r * trust::RANGE_STRIDE + off) as u64
    }

    fn config(&self, aml_size: usize) -> TrustdConfig {
        let mut ranges = [trust::Range::default(); trust::MAX_RANGES];
        for (r, &(paddr, size)) in self.ranges.iter().enumerate() {
            ranges[r] = trust::Range { paddr: paddr as u64, size: size as u64 };
        }
        let va = |x: Option<(usize, usize)>| x.map_or(0, Self::va);
        let buf = |x: Option<(usize, usize, usize)>| x.map_or((0, 0), |(r, off, n)| (Self::va((r, off)), n as u64));
        let (cmd_va, cmd_size) = buf(self.cmd);
        let (rsp_va, rsp_size) = buf(self.rsp);
        TrustdConfig {
            iface: self.iface,
            start: self.start,
            ranges,
            loc_va: va(self.loc),
            ctrl_va: va(self.ctrl),
            cmd_va,
            cmd_size,
            rsp_va,
            rsp_size,
            start_va: va(self.start_bell),
            reply_va: va(self.reply_bell),
            aml_size: aml_size as u64,
        }
    }
}

/// The TPM, from its ACPI table (DESIGN.md §9.2).
#[cfg(target_arch = "x86_64")]
fn find_tpm(bootinfo: &sel4::BootInfoPtr, phys: &mut crate::acpi::Phys) -> Option<Layout> {
    use crate::blkd::DEVICE_ATTRS;
    /// TPM 2.0 at its standard address (PC Client PTP), where the ACPI
    /// table gives none (FIFO interface).
    const TPM_DEFAULT_BASE: usize = 0xfed4_0000;
    // The TPM2 table, up to the Pluton doorbells (revision 4 is 76 bytes).
    let mut buf = [0; 76];
    let Some(t) = crate::acpi::find(bootinfo, phys, b"TPM2", &mut buf).filter(|t| t.len() >= 52) else {
        info!("l4h-root: no TPM, no attestation");
        return None;
    };
    let u64_at = |i: usize| u64::from_le_bytes(t[i..i + 8].try_into().unwrap()) as usize;
    let control = u64_at(40);
    let method = u32::from_le_bytes(t[48..52].try_into().unwrap());
    let mut l = Layout::default();
    let unusable = |why: &str| -> Option<Layout> {
        sel4::debug_println!("l4h-root: TPM (start method {}) unusable, no attestation: {}", method, why);
        None
    };
    if method == 6 {
        // FIFO ("TIS"): locality 0's registers.
        l.iface = trust::TPM_TIS;
        l.loc = l.range(if control != 0 { control & !(PAGE - 1) } else { TPM_DEFAULT_BASE }, PAGE);
        return Some(l);
    }
    l.iface = trust::TPM_CRB;
    l.start = match method {
        7 => trust::START_CRB,
        2 | 8 => trust::START_ACPI,
        13 => trust::START_PLUTON,
        _ => return unusable("unsupported start method"),
    };
    if control == 0 {
        return unusable("no control area");
    }
    // Only plain CRB has locality registers, the page the control area is
    // at 0x40 of (as Linux tpm_crb).
    if method == 7 {
        if control % PAGE != 0x40 {
            return unusable("control area not in the locality page");
        }
        l.loc = l.range(control & !(PAGE - 1), PAGE);
    }
    // The control area and the buffers it names.
    let mut regs = [0; 0x30];
    phys.read_into_attrs(control, &mut regs, DEVICE_ATTRS)?;
    let w32 = |i: usize| u32::from_le_bytes(regs[i..i + 4].try_into().unwrap()) as usize;
    let (cmd, cmd_size) = (w32(0x1c) | w32(0x20) << 32, w32(0x18));
    let (rsp, rsp_size) = (w32(0x28) | w32(0x2c) << 32, w32(0x24));
    if cmd == 0 || rsp == 0 || cmd_size < 10 || rsp_size < 10 {
        return unusable("no command or response buffer");
    }
    l.ctrl = l.range(control, 0x30);
    l.cmd = l.range(cmd, cmd_size).map(|(r, off)| (r, off, cmd_size));
    l.rsp = l.range(rsp, rsp_size).map(|(r, off)| (r, off, rsp_size));
    if method == 13 {
        // Pluton: the doorbells' addresses (struct tpm2_crb_pluton in
        // Linux) right after the table's fixed part, in place of the
        // start method parameters.
        if t.len() < 52 + 16 {
            return unusable("no Pluton doorbells");
        }
        l.start_bell = l.range(u64_at(52), 4);
        l.reply_bell = l.range(u64_at(60), 4);
    }
    if l.ctrl.is_none() || l.cmd.is_none() || l.rsp.is_none() || (method == 13 && l.reply_bell.is_none()) {
        return unusable("registers or buffers too large");
    }
    Some(l)
}

/// Start trustd if the host has a TPM (and root was built with trustd).
pub fn spawn(
    bootinfo: &sel4::BootInfoPtr,
    alloc: &mut Alloc,
    devmem: &mut DevMem,
    win: &mut Window,
    asids: &mut AsidPools,
    info: &HostInfo,
    #[cfg(target_arch = "x86_64")]
    frames: crate::acpi::Frames,
) -> Option<Trustd> {
    if TRUSTD_ELF.is_empty() {
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    {
        let mut frames = frames;
        let mut phys = crate::acpi::Phys::new(alloc, devmem, win, &mut frames);
        let l = find_tpm(bootinfo, &mut phys)?;
        // The pages of each range, which stay in `frames`.
        for &(base, size) in &l.ranges {
            if (0..size / PAGE).any(|p| phys.frame(base + p * PAGE).is_none()) {
                sel4::debug_println!("l4h-root: TPM at {:#x} is not device memory, no attestation", base);
                return None;
            }
        }
        // The AML's size.  Its pages are carved now, so that copying it
        // once trustd exists cannot fail for want of them.
        let aml_size = match l.start {
            trust::START_ACPI => {
                let touch = |phys: &mut crate::acpi::Phys, pa: usize, len: usize| {
                    (pa & !(PAGE - 1)..pa + len).step_by(PAGE).try_for_each(|p| phys.frame(p).map(drop))
                };
                match crate::acpi::aml_tables(bootinfo, &mut phys, touch) {
                    Some(n) => n,
                    None => {
                        sel4::debug_println!("l4h-root: cannot read the ACPI tables for the TPM, no attestation");
                        return None;
                    }
                }
            }
            _ => 0,
        };
        info!(
            "l4h-root: TPM 2.0 ({}) at {:#x}",
            match (l.iface, l.start) {
                (trust::TPM_TIS, _) => "FIFO",
                (_, trust::START_ACPI) => "CRB, ACPI start",
                (_, trust::START_PLUTON) => "CRB, Pluton",
                _ => "CRB",
            },
            l.ranges[0].0
        );
        let cfg = l.config(aml_size);
        let (comp, arena, keep) = create(&cfg, alloc, win, asids, info, aml_size)?;
        let mut keep_next = 0;
        let mut keep_slot = || {
            keep_next += 1;
            keep.absolute_cptr_from_bits_with_depth(keep_next - 1, KEEP_BITS)
        };
        let mut arena = arena;
        for (r, &(base, size)) in l.ranges.iter().enumerate() {
            for p in 0..size / PAGE {
                let f = frames.get(base + p * PAGE).expect("TPM page carved above");
                let va = trust::TPM_VA + r * trust::RANGE_STRIDE + p * PAGE;
                comp.map_copy(alloc, &mut arena, &abs_of(f), va, CapRights::read_write(), crate::blkd::DEVICE_ATTRS, keep_slot());
            }
        }
        // The ACPI tables, copied into fresh frames a piece at a time, as
        // trustd sees them: back to back from AML_VA.
        if aml_size != 0 {
            let mut at = 0;
            let mut frame = None;
            let copy = |phys: &mut crate::acpi::Phys, pa: usize, len: usize| {
                let mut buf = [0; 512];
                let mut done = 0;
                while done < len {
                    if at % PAGE == 0 {
                        let f = arena.object_cap::<cap_type::Granule>(phys.alloc, FRAME_BP);
                        comp.map_copy(phys.alloc, &mut arena, &abs_of(f), trust::AML_VA + at, CapRights::read_only(), VmAttributes::DEFAULT, keep_slot());
                        frame = Some(f);
                    }
                    let n = (len - done).min(PAGE - at % PAGE).min(buf.len());
                    phys.read_into(pa + done, &mut buf[..n])?;
                    let off = at % PAGE;
                    phys.win.with_frame(&abs_of(frame?), |dst| unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), dst.add(off), n) });
                    (done, at) = (done + n, at + n);
                }
                Some(())
            };
            let mut phys = crate::acpi::Phys::new(alloc, devmem, win, &mut frames);
            if crate::acpi::aml_tables(bootinfo, &mut phys, copy) != Some(aml_size) {
                sel4::debug_println!("l4h-root: cannot copy the ACPI tables for the TPM, no attestation");
                alloc.reclaim(arena);
                return None;
            }
        }
        finish(comp, arena, alloc, &mut keep_slot, frames)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (bootinfo, alloc, devmem, win, asids, info);
        None
    }
}

/// Create trustd with configuration `cfg`, and the CNode that keeps its
/// mapping capabilities.
fn create(
    cfg: &TrustdConfig,
    alloc: &mut Alloc,
    win: &mut Window,
    asids: &mut AsidPools,
    info: &HostInfo,
    aml_size: usize,
) -> Option<(Component, Arena, cap::CNode)> {
    let mut arena = alloc.arena((8 << 20) + 2 * aml_size)?;
    let cfg_bytes = unsafe {
        core::slice::from_raw_parts(cfg as *const TrustdConfig as *const u8, core::mem::size_of::<TrustdConfig>())
    };
    let args = SpawnArgs {
        id: TRUSTD_ID,
        kind: host::KIND_TRUSTD,
        elf: TRUSTD_ELF,
        core: info.cores - 1,
        prio: abi::PRIO_KERNEL + 1,
        // At most a quarter of a core, whatever the TPM or its firmware's
        // AML does (trustd sleeps between polls of the TPM).
        budget_us: info.timeslice_us,
        period_us: 4 * info.timeslice_us,
        config: cfg_bytes,
        ntfn: None,
        service_ep: None,
        cnode: None,
        cnode_bits: None,
    };
    let comp = component::create(&args, alloc, &mut arena, win, asids, info);
    let keep = arena.object_cap::<cap_type::CNode>(alloc, ObjectBlueprint::CNode { size_bits: KEEP_BITS });
    Some((comp, arena, keep))
}

/// Map the registry and request pages, give trustd its kick notification,
/// and start it.
#[cfg(target_arch = "x86_64")]
fn finish(
    comp: Component,
    mut arena: Arena,
    alloc: &mut Alloc,
    keep: &mut impl FnMut() -> sel4::AbsoluteCPtr,
    frames: crate::acpi::Frames,
) -> Option<Trustd> {
    let registry = alloc.object_cap::<cap_type::Granule>(FRAME_BP);
    alloc.global(|alloc| {
        let copy = alloc.slots.alloc();
        abs_of(sel4::init_thread::Slot::<cap_type::Null>::from_index(copy).cap())
            .copy(&abs_of(registry), CapRights::all())
            .expect("registry copy");
        let f = sel4::init_thread::Slot::<cap_type::Granule>::from_index(copy).cap();
        map_frame(&mut ObjCtx::global(alloc), sel4::init_thread::slot::VSPACE.cap(), f, REGISTRY_VA, CapRights::read_write(), VmAttributes::DEFAULT);
    });
    comp.map_copy(alloc, &mut arena, &abs_of(registry), trust::REGISTRY_VA, CapRights::read_only(), VmAttributes::DEFAULT, keep());
    let pages = core::array::from_fn(|g| {
        let f = arena.object_cap::<cap_type::Granule>(alloc, FRAME_BP);
        comp.map_copy(alloc, &mut arena, &abs_of(f), trust::page_va(g), CapRights::read_write(), VmAttributes::DEFAULT, keep());
        f
    });
    let kick = arena.object_cap::<cap_type::Notification>(alloc, ObjectBlueprint::Notification);
    comp.give(trust::SLOT_KICK, kick, CapRights::all());
    component::start(&comp);
    Some(Trustd { comp, arena, registry: REGISTRY_VA as *mut GuestRecord, next_instance: 1, pages, kick, frames })
}

impl Trustd {
    /// Give guest `g` (being created) its attestation device: its id's
    /// request page, cleared, and a kick capability badged with its id.
    pub fn give(&self, g: &mut Guest, alloc: &mut Alloc, win: &mut Window) {
        let page = self.pages[g.id];
        win.with_frame(&abs_of(page), |p| unsafe { core::ptr::write_bytes(p, 0, PAGE) });
        if g.add_device_with(alloc, crate::guest::DevKind::Attest, page, (self.kick, trust::kick_badge(g.id))).is_some() {
            g.attest = true;
        }
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

    /// trustd faulted (the firmware's AML can make it panic): stop it,
    /// mark every guest's page failed and answer what it asked with -EIO,
    /// so that no guest waits for good.  New guests get no attestation.
    fn fail(self, h: &mut Host) {
        let _ = self.comp.tcb.tcb_suspend();
        let kick = abs_of(self.kick);
        let _ = kick.revoke();
        for id in 0..h.guests.len() {
            if h.guest(id).is_none_or(|g| !g.attest) {
                continue;
            }
            h.wins.a.with_frame(&abs_of(self.pages[id]), |p| unsafe {
                let p = p as *mut abi::AttestPage;
                core::ptr::addr_of_mut!((*p).state).write_volatile(abi::ATTEST_FAILED);
                let seq = core::ptr::addr_of!((*p).seq).read_volatile();
                core::ptr::addr_of_mut!((*p).status).write_volatile(-(crate::ctl::EIO as i32));
                core::ptr::addr_of_mut!((*p).total).write_volatile(0);
                core::ptr::addr_of_mut!((*p).out_len).write_volatile(0);
                core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
                core::ptr::addr_of_mut!((*p).done).write_volatile(seq);
            });
            done(h, id);
        }
        sel4::debug_println!("l4h-root: trustd failed, no attestation");
    }
}

/// A fault of trustd (component `id`)?  Then it is gone.
pub fn fault(h: &mut Host, id: u32) {
    if id == TRUSTD_ID {
        if let Some(t) = h.trustd.take() {
            t.fail(h);
        }
    }
}

/// ROOT_CALL_ATTEST_DONE: trustd answered guest `id`; raise the event of
/// its attestation device.
pub fn done(h: &mut Host, id: usize) {
    let Some(g) = h.guests.get(id).and_then(|g| g.as_ref()) else { return };
    if let Some(d) = g.devices.iter().position(|d| d.kind == crate::guest::DevKind::Attest) {
        g.raise_event(&mut h.alloc, crate::guest::dev_event(d));
    }
}

/// ROOT_CALL_HW from trustd: one access the firmware's AML makes
/// (trust::HW_*).  Returns what it read (all ones when refused).
#[cfg(target_arch = "x86_64")]
pub fn hw(h: &mut Host) -> u64 {
    let (op, addr, off, width, value) = (mr(0), mr(1) as usize, mr(2) as usize, mr(3) as usize, mr(4));
    let write = op & 1 != 0;
    let r = match op {
        trust::HW_MEM_READ | trust::HW_MEM_WRITE => {
            let Some(t) = h.trustd.as_mut() else { return u64::MAX };
            crate::acpi::Phys::new(&mut h.alloc, &mut h.devmem, &mut h.wins.a, &mut t.frames).access(addr, width, write, value)
        }
        trust::HW_IO_READ | trust::HW_IO_WRITE => io(&mut h.alloc, addr, width, write, value),
        trust::HW_PCI_READ | trust::HW_PCI_WRITE => {
            let (bus, dev, func) = ((addr >> 8) as u8, (addr >> 3 & 0x1f) as u8, (addr & 7) as u8);
            let driven = h.ctrls.iter().map(|c| &c.pci).chain(h.netd.nics.iter().map(|n| &n.pci))
                .any(|d| (d.bus, d.dev, d.func) == (bus, dev, func));
            if addr >> 16 != 0 || off + width > 256 || off % width != 0 || !matches!(width, 1 | 2 | 4) || (write && driven) {
                None
            } else {
                let (word, shift) = (off as u8 & !3, (off % 4) * 8);
                let old = h.pci.read32(bus, dev, func, word);
                let mask = if width == 4 { u32::MAX } else { ((1u32 << (8 * width)) - 1) << shift };
                if write {
                    h.pci.write32(bus, dev, func, word, (old & !mask) | ((value as u32) << shift & mask));
                }
                Some(((old & mask) >> shift) as u64)
            }
        }
        _ => None,
    };
    r.unwrap_or_else(|| {
        info!("l4h-root: trustd: refused hardware access {} at {:#x}+{:#x}", op, addr, off);
        u64::MAX
    })
}

#[cfg(not(target_arch = "x86_64"))]
pub fn hw(_h: &mut Host) -> u64 {
    u64::MAX
}

/// An I/O port access, through a capability for just those ports: seL4
/// refuses ports already given out (the serial port, PCI configuration,
/// legacy NICs).
#[cfg(target_arch = "x86_64")]
fn io(alloc: &mut Alloc, port: usize, width: usize, write: bool, value: u64) -> Option<u64> {
    if !matches!(width, 1 | 2 | 4) || port + width > 0x1_0000 {
        return None;
    }
    let cap = crate::pci::io_ports(alloc, port as u16, width as u16)?;
    let (c, p) = (cap.bits(), port as u16);
    let r = sel4::with_ipc_buffer_mut(|ipc| {
        let ipc = ipc.inner_mut();
        match (width, write) {
            (1, false) => ipc.seL4_X86_IOPort_In8(c, p).result as u64,
            (2, false) => ipc.seL4_X86_IOPort_In16(c, p).result as u64,
            (_, false) => ipc.seL4_X86_IOPort_In32(c, p).result as u64,
            (1, true) => {
                ipc.seL4_X86_IOPort_Out8(c, p as u64, value);
                0
            }
            (2, true) => {
                ipc.seL4_X86_IOPort_Out16(c, p as u64, value);
                0
            }
            (_, true) => {
                ipc.seL4_X86_IOPort_Out32(c, p as u64, value);
                0
            }
        }
    });
    let _ = abs_of(cap).delete();
    alloc.slots.free(cap.bits() as usize);
    Some(r)
}
