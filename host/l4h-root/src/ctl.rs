//! The control API for dom0 (DESIGN.md §9.1): requests arrive in dom0's
//! control page (struct l4h_ctl_req), the response is written next to it.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_abi as abi;
use l4h_host as host;

use crate::guest::{GuestConfig, State};
use crate::{Host, MAX_GUESTS};

pub const EPERM: i64 = 1;
pub const ENOENT: i64 = 2;
pub const ENOMEM: i64 = 12;
pub const EBUSY: i64 = 16;
pub const EINVAL: i64 = 22;
pub const ENOSPC: i64 = 28;
pub const ENOSYS: i64 = 38;

/// Execute the request in the control page of guest `caller` (dom0).
pub fn handle(h: &mut Host, caller: usize) {
    let Some(page) = h.guest(caller).and_then(|g| g.ctl_page()) else { return };
    let req = unsafe { (page as *const abi::CtlReq).read_volatile() };
    let mut resp = abi::CtlResp { status: 0, vals: [0; 6], data: [0; abi::CTL_DATA_MAX] };
    resp.status = if req.version != abi::CTL_VERSION {
        -EINVAL
    } else {
        match execute(h, caller, &req, &mut resp) {
            Ok(()) => 0,
            Err(e) => -e,
        }
    };
    unsafe { (page.add(abi::CTL_RESP_OFF) as *mut abi::CtlResp).write_volatile(resp) };
}

fn vm_arg(a: u64) -> Result<usize, i64> {
    let id = a as usize;
    if id >= MAX_GUESTS { Err(ENOENT) } else { Ok(id) }
}

fn cstr(data: &[u8]) -> &[u8] {
    let n = data.iter().position(|&c| c == 0).unwrap_or(data.len());
    &data[..n]
}

fn execute(h: &mut Host, caller: usize, req: &abi::CtlReq, resp: &mut abi::CtlResp) -> Result<(), i64> {
    let a = &req.args;
    match req.op {
        abi::CTL_HOST_INFO => {
            resp.vals = [
                h.info.cores as u64,
                h.alloc.free_bytes() as u64,
                h.total_bytes as u64,
                h.disks.len() as u64,
                h.guests.iter().flatten().count() as u64,
                h.netd.nics.len() as u64,
            ];
            Ok(())
        }
        abi::CTL_DISK_INFO => {
            let d = h.disks.get(a[0] as usize).ok_or(ENOENT)?;
            let p = &d.pci;
            resp.vals[0] = ((p.bus as u64) << 8) | ((p.dev as u64) << 3) | p.func as u64;
            resp.vals[1] = d.owner.map_or(u64::MAX, |o| o as u64);
            resp.vals[3] = d.kind as u64;
            Ok(())
        }
        abi::CTL_VM_CREATE => {
            let name = core::str::from_utf8(cstr(&req.data[..abi::CTL_NAME_MAX])).map_err(|_| EINVAL)?;
            let (ram, vcpus) = (a[0] as usize, a[1] as usize);
            if vcpus == 0 || vcpus > h.info.cores * 4 {
                return Err(EINVAL);
            }
            let cfg = GuestConfig {
                name,
                ram,
                vcpus,
                // seL4 objects of the guest kernel: mostly mapping slots
                // and page tables of its processes.
                untyped_quota: if a[3] != 0 { a[3] as usize } else { (16 << 20) + ram / 16 },
                vspaces: 64,
                ram_base: a[2] as usize,
                dom0: false,
            };
            let id = h.create_guest(&cfg).ok_or(ENOMEM)?;
            resp.vals[0] = id as u64;
            Ok(())
        }
        abi::CTL_VM_LOAD => {
            let id = vm_arg(a[0])?;
            let (dst, src, len) = (a[1] as usize, a[2] as usize, a[3] as usize);
            if id == caller || len as u64 > abi::CTL_LOAD_MAX {
                return Err(EINVAL);
            }
            let g = h.guests.get(id).and_then(|g| g.as_ref()).ok_or(ENOENT)?;
            if g.state != State::Created || g.vmm.is_some() {
                return Err(EBUSY);
            }
            let d0 = h.guests.get(caller).and_then(|g| g.as_ref()).ok_or(ENOENT)?;
            if g.copy_ram(&mut h.wins, dst, d0, src, len) { Ok(()) } else { Err(EINVAL) }
        }
        abi::CTL_VM_START => {
            let id = vm_arg(a[0])?;
            if id == caller {
                return Err(EINVAL);
            }
            let cmdline = cstr(&req.data);
            let ok = h
                .with_guest(id, |h, g| {
                    g.state == State::Created
                        && g.vmm.is_none()
                        && g.write_boot_info(&mut h.wins.a, &h.info, a[1], a[2] as usize, (a[3] as usize, a[4] as usize), cmdline)
                })
                .ok_or(ENOENT)?;
            if !ok {
                return Err(EINVAL);
            }
            h.start_guest(id)
        }
        abi::CTL_VM_DESTROY => {
            let id = vm_arg(a[0])?;
            if id == caller {
                return Err(EPERM);
            }
            h.destroy_guest(id)
        }
        abi::CTL_VM_LIST => {
            let first = a[0] as usize;
            let per = abi::CTL_DATA_MAX / core::mem::size_of::<abi::CtlVm>();
            let mut n = 0;
            for g in h.guests.iter().skip(first).flatten() {
                if n == per {
                    break;
                }
                let mut vm = abi::CtlVm {
                    id: g.id as u32,
                    state: match g.state {
                        State::Created => abi::VM_CREATED,
                        State::Running => abi::VM_RUNNING,
                        State::Stopped => abi::VM_STOPPED,
                    },
                    vcpus: g.vcpus.len() as u32,
                    reserved: 0,
                    ram: g.ram_size() as u64,
                    name: [0; abi::CTL_NAME_MAX],
                };
                let nb = g.name.as_bytes();
                let k = nb.len().min(abi::CTL_NAME_MAX - 1);
                vm.name[..k].copy_from_slice(&nb[..k]);
                let sz = core::mem::size_of::<abi::CtlVm>();
                unsafe {
                    core::ptr::copy_nonoverlapping(&vm as *const abi::CtlVm as *const u8, resp.data.as_mut_ptr().add(n * sz), sz);
                }
                n += 1;
            }
            resp.vals[0] = n as u64;
            Ok(())
        }
        abi::CTL_BLK_ATTACH => {
            let id = vm_arg(a[0])?;
            if id == caller {
                return Err(EINVAL);
            }
            h.attach_disk(id, a[1] as usize, a[2] != 0, a[3], a[4])
        }
        abi::CTL_CONSOLE_READ => {
            let g = h.guest(vm_arg(a[0])?).ok_or(ENOENT)?;
            let ring = g.console_ring();
            let head = unsafe { addr_of!((*ring).head).read_volatile() };
            fence(Ordering::Acquire);
            let off = a[1].min(head);
            let start = off.max(head.saturating_sub(host::CONSOLE_RING_DATA as u64));
            let n = (head - start).min(abi::CTL_DATA_MAX as u64) as usize;
            for i in 0..n {
                let idx = (start as usize + i) % host::CONSOLE_RING_DATA;
                resp.data[i] = unsafe { addr_of!((*ring).data[idx]).read_volatile() };
            }
            resp.vals = [start + n as u64, n as u64, start - off, 0, 0, 0];
            Ok(())
        }
        abi::CTL_CONSOLE_WRITE => {
            let g = h.guest(vm_arg(a[0])?).ok_or(ENOENT)?;
            let len = (a[1] as usize).min(abi::CTL_DATA_MAX);
            let p = g.console_page();
            unsafe {
                let mut head = addr_of!((*p).in_head).read_volatile();
                let tail = addr_of!((*p).in_tail).read_volatile();
                // Both indices are in the guest's page: the guest may have
                // written anything there.
                let room = abi::CONSOLE_IN_SIZE.saturating_sub(head.wrapping_sub(tail) as usize);
                let n = len.min(room);
                for &c in &req.data[..n] {
                    addr_of_mut!((*p).input[head as usize % abi::CONSOLE_IN_SIZE]).write_volatile(c);
                    head = head.wrapping_add(1);
                }
                fence(Ordering::Release);
                addr_of_mut!((*p).in_head).write_volatile(head);
                resp.vals[0] = n as u64;
            }
            Ok(())
        }
        abi::CTL_SWITCH_CREATE => {
            resp.vals[0] = h.netd.create_switch() as u64;
            Ok(())
        }
        abi::CTL_SWITCH_ATTACH_UPLINK => h.netd.attach_uplink(a[0] as u32, a[1] as usize),
        abi::CTL_NIC_CREATE => {
            let id = vm_arg(a[0])?;
            if id == caller {
                return Err(EINVAL);
            }
            resp.vals[0] = h.create_nic(id, a[1] as u32, a[2], a[3] & abi::NIC_NO_MAC_LOCK == 0)?;
            Ok(())
        }
        abi::CTL_NIC_INFO => {
            let n = h.netd.nics.get(a[0] as usize).ok_or(ENOENT)?;
            let p = &n.pci;
            resp.vals[0] = ((p.bus as u64) << 8) | ((p.dev as u64) << 3) | p.func as u64;
            resp.vals[1] = n.switch.map_or(u64::MAX, |s| s as u64);
            resp.vals[2] = h.netd.nic_mac(a[0] as usize).unwrap_or(0);
            Ok(())
        }
        abi::CTL_SWITCH_INFO => {
            let sw = a[0] as u32;
            if sw >= h.netd.switches {
                return Err(ENOENT);
            }
            let sz = core::mem::size_of::<abi::CtlPort>();
            let mut n = 0;
            for (p, info) in h.netd.ports.iter().enumerate() {
                let Some(info) = info else { continue };
                if info.switch != sw || (n + 1) * sz > abi::CTL_DATA_MAX {
                    continue;
                }
                let (rx_frames, tx_frames, drops) = h.netd.port_stats(p);
                let e = abi::CtlPort { vm: info.vm as u32, port: p as u32, mac: info.mac, rx_frames, tx_frames, drops };
                unsafe {
                    core::ptr::copy_nonoverlapping(&e as *const abi::CtlPort as *const u8, resp.data.as_mut_ptr().add(n * sz), sz);
                }
                n += 1;
            }
            resp.vals[0] = n as u64;
            resp.vals[1] = h.netd.nics.iter().position(|n| n.switch == Some(sw)).map_or(u64::MAX, |i| i as u64);
            Ok(())
        }
        abi::CTL_VM_SET_SCHED => {
            let id = vm_arg(a[0])?;
            if a[1] > 100 {
                return Err(EINVAL);
            }
            if crate::sched::set_cap(h, id, a[1] as u32) { Ok(()) } else { Err(ENOENT) }
        }
        abi::CTL_VM_SHUTDOWN => {
            let id = vm_arg(a[0])?;
            if a[1] != abi::POWER_OFF && a[1] != abi::POWER_REBOOT {
                return Err(EINVAL);
            }
            let g = h.guests.get(id).and_then(|g| g.as_ref()).ok_or(ENOENT)?;
            if g.state != State::Running {
                return Err(EBUSY);
            }
            unsafe { addr_of_mut!((*g.console_page()).power_request).write_volatile(a[1] as u32 + 1) };
            // The console is device 0.
            g.raise_event(&mut h.alloc, crate::guest::dev_event(0));
            Ok(())
        }
        abi::CTL_VM_STATS => {
            let g = h.guest(vm_arg(a[0])?).ok_or(ENOENT)?;
            resp.vals[0] = g.vcpus.iter().map(|v| v.usage as u64).sum();
            resp.vals[1] = g.cpu_cap_pct as u64;
            Ok(())
        }
        abi::CTL_VM_INFO => {
            let g = h.guest(vm_arg(a[0])?).ok_or(ENOENT)?;
            for (v, vcpu) in g.vcpus.iter().take(6).enumerate() {
                resp.vals[v] = vcpu.core as u64;
            }
            Ok(())
        }
        _ => Err(ENOSYS),
    }
}
