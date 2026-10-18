//! l4hctl: dom0 client library of the l4h control API (DESIGN.md §9.1), on
//! /dev/l4hctl.  The request layouts mirror `include/l4h/abi.h`.  The
//! `l4hctl` binary is a command-line front end; other dom0 programs (such
//! as an orchestration agent) link this library.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub const CTL_VERSION: u32 = 1;
pub const CTL_DATA_MAX: usize = 1984;
pub const CTL_LOAD_MAX: usize = 1 << 20;
pub const CTL_NAME_MAX: usize = 32;

pub const CTL_HOST_INFO: u32 = 1;
pub const CTL_VM_CREATE: u32 = 2;
pub const CTL_VM_LOAD: u32 = 3;
pub const CTL_VM_START: u32 = 4;
pub const CTL_VM_DESTROY: u32 = 5;
pub const CTL_VM_LIST: u32 = 6;
pub const CTL_BLK_ATTACH: u32 = 7;
pub const CTL_CONSOLE_READ: u32 = 8;
pub const CTL_CONSOLE_WRITE: u32 = 9;
pub const CTL_DISK_INFO: u32 = 10;
pub const CTL_SWITCH_CREATE: u32 = 11;
pub const CTL_SWITCH_ATTACH_UPLINK: u32 = 12;
pub const CTL_NIC_CREATE: u32 = 13;
pub const CTL_SWITCH_INFO: u32 = 14;
pub const CTL_NIC_INFO: u32 = 15;
pub const NIC_NO_MAC_LOCK: u64 = 1;
pub const NIC_ISOLATED: u64 = 2;
pub const CTL_VM_SET_SCHED: u32 = 16;
pub const CTL_VM_INFO: u32 = 17;
pub const CTL_VM_STATS: u32 = 18;
pub const CTL_VM_SHUTDOWN: u32 = 19;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CtlPort {
    pub vm: u32,
    pub port: u32,
    pub mac: u64,
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub drops: u64,
}

pub fn fmt_mac(m: u64) -> String {
    let b = m.to_be_bytes();
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[2], b[3], b[4], b[5], b[6], b[7])
}

pub fn parse_mac(s: &str) -> Result<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return err(format!("bad MAC: {s}"));
    }
    let mut m = 0u64;
    for p in parts {
        m = (m << 8) | u8::from_str_radix(p, 16).map_err(|_| Error(format!("bad MAC: {s}")))? as u64;
    }
    Ok(m)
}

#[repr(C)]
pub struct Req {
    pub op: u32,
    pub version: u32,
    pub args: [u64; 6],
    pub data: [u8; CTL_DATA_MAX],
}

#[repr(C)]
pub struct Resp {
    pub status: i64,
    pub vals: [u64; 6],
    pub data: [u8; CTL_DATA_MAX],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CtlVm {
    pub id: u32,
    pub state: u32,
    pub vcpus: u32,
    pub reserved: u32,
    pub ram: u64,
    pub name: [u8; CTL_NAME_MAX],
}

#[repr(C)]
pub struct Call {
    req: u64,
    resp: u64,
    buf: u64,
    len: u64,
}

// _IOWR('L', 1, struct l4hctl_call)
pub const L4HCTL_IOC_CALL: u64 = (3 << 30) | ((std::mem::size_of::<Call>() as u64) << 16) | ((b'L' as u64) << 8) | 1;

unsafe extern "C" {
    fn ioctl(fd: i32, req: u64, ...) -> i32;
}

pub struct Ctl {
    dev: File,
}

#[derive(Debug)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

pub fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

pub fn errno_name(e: i64) -> &'static str {
    match e {
        1 => "operation not permitted",
        2 => "no such VM or device",
        5 => "I/O error (the disk failed to reset)",
        12 => "out of memory",
        16 => "busy",
        22 => "invalid argument",
        28 => "no space for devices",
        38 => "not supported",
        _ => "error",
    }
}

impl Ctl {
    pub fn open() -> Result<Self> {
        File::options()
            .read(true)
            .write(true)
            .open("/dev/l4hctl")
            .map(|dev| Ctl { dev })
            .map_err(|e| Error(format!("/dev/l4hctl: {e} (not dom0?)")))
    }

    pub fn call(&self, op: u32, args: &[u64], data: &[u8], buf: &[u8]) -> Result<Box<Resp>> {
        let mut req = Box::new(Req { op, version: CTL_VERSION, args: [0; 6], data: [0; CTL_DATA_MAX] });
        req.args[..args.len()].copy_from_slice(args);
        req.data[..data.len()].copy_from_slice(data);
        let mut resp = Box::new(Resp { status: 0, vals: [0; 6], data: [0; CTL_DATA_MAX] });
        let call = Call {
            req: &*req as *const Req as u64,
            resp: &mut *resp as *mut Resp as u64,
            buf: buf.as_ptr() as u64,
            len: buf.len() as u64,
        };
        let r = unsafe { ioctl(self.dev.as_raw_fd(), L4HCTL_IOC_CALL, &call as *const Call) };
        if r < 0 {
            return err(format!("ioctl: {}", io::Error::last_os_error()));
        }
        if resp.status < 0 {
            return err(format!("request {op} failed: {}", errno_name(-resp.status)));
        }
        Ok(resp)
    }
}

/// The parts of a guest kernel ELF needed to load it.
pub struct Kernel {
    pub entry: u64,
    pub ram_base: u64,
    pub end_gpa: u64,
    /// (guest-physical address, bytes)
    pub segments: Vec<(u64, Vec<u8>)>,
}

pub fn rd<const N: usize>(b: &[u8], off: usize) -> Result<[u8; N]> {
    b.get(off..off + N).map(|s| s.try_into().unwrap()).ok_or(Error("truncated ELF".into()))
}

pub fn parse_kernel(b: &[u8]) -> Result<Kernel> {
    if b.len() < 64 || &b[..4] != b"\x7fELF" || b[4] != 2 || b[5] != 1 {
        return err("not a 64-bit little-endian ELF");
    }
    let entry = u64::from_le_bytes(rd(b, 24)?);
    let phoff = u64::from_le_bytes(rd(b, 32)?) as usize;
    let phentsize = u16::from_le_bytes(rd(b, 54)?) as usize;
    let phnum = u16::from_le_bytes(rd(b, 56)?) as usize;
    let mut loads = Vec::new();
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if u32::from_le_bytes(rd(b, p)?) != 1 {
            continue;
        }
        let off = u64::from_le_bytes(rd(b, p + 8)?) as usize;
        let vaddr = u64::from_le_bytes(rd(b, p + 16)?);
        let filesz = u64::from_le_bytes(rd(b, p + 32)?) as usize;
        let memsz = u64::from_le_bytes(rd(b, p + 40)?);
        let data = b.get(off..off + filesz).ok_or(Error("segment beyond end of file".into()))?.to_vec();
        loads.push((vaddr, memsz, data));
    }
    let ram_base = loads.iter().map(|l| l.0).min().ok_or(Error("no loadable segments".into()))? & !0xfff;
    let end = loads.iter().map(|l| l.0 + l.1).max().unwrap();
    Ok(Kernel {
        entry,
        ram_base,
        end_gpa: (end - ram_base + 0xfff) & !0xfff,
        segments: loads.into_iter().map(|(v, _, d)| (v - ram_base, d)).collect(),
    })
}

pub fn load(ctl: &Ctl, vm: u64, gpa: u64, data: &[u8]) -> Result<()> {
    for (i, chunk) in data.chunks(CTL_LOAD_MAX).enumerate() {
        ctl.call(CTL_VM_LOAD, &[vm, gpa + (i * CTL_LOAD_MAX) as u64], &[], chunk)?;
    }
    Ok(())
}

pub fn read_file(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error(format!("{path}: {e}")))
}

pub fn parse_num(s: &str) -> Result<u64> {
    s.parse().map_err(|_| Error(format!("bad number: {s}")))
}

/// A size in MiB, as bytes.
pub fn parse_mib(s: &str) -> Result<u64> {
    parse_num(s)?.checked_mul(1 << 20).ok_or(Error(format!("too large: {s} MiB")))
}

/// A VM to create and start with [`Ctl::create_vm`].
pub struct VmSpec<'a> {
    pub name: &'a str,
    /// RAM in bytes.
    pub mem: u64,
    pub vcpus: u64,
    /// The guest kernel ELF.
    pub kernel: &'a [u8],
    /// The initramfs (several concatenated archives are fine); may be empty.
    pub initrd: &'a [u8],
    pub cmdline: &'a str,
    /// (disk, read-only, first sector, sectors)
    pub disks: Vec<(u64, bool, u64, u64)>,
    /// I/O pool in bytes (0: default, 4 MiB; a power of two, 1-256 MiB).
    pub io_pool: u64,
    /// (switch, MAC or 0, NIC_* flags)
    pub nics: Vec<(u64, u64, u64)>,
}

/// A VM as listed by [`Ctl::vm_list`].
#[derive(Clone, Debug)]
pub struct Vm {
    pub id: u64,
    /// VM_CREATED, VM_RUNNING or VM_STOPPED.
    pub state: u32,
    pub vcpus: u32,
    pub ram: u64,
    pub name: String,
}

pub const VM_CREATED: u32 = 1;
pub const VM_RUNNING: u32 = 2;
pub const VM_STOPPED: u32 = 3;
pub const POWER_OFF: u64 = 0;
pub const POWER_REBOOT: u64 = 1;

/// Host resources, from [`Ctl::host_info`].
#[derive(Clone, Copy, Debug)]
pub struct HostInfo {
    pub cores: u64,
    pub free: u64,
    pub total: u64,
    pub disks: u64,
    pub vms: u64,
    pub nics: u64,
}

impl Ctl {
    pub fn host_info(&self) -> Result<HostInfo> {
        let r = self.call(CTL_HOST_INFO, &[], &[], &[])?;
        let v = r.vals;
        Ok(HostInfo { cores: v[0], free: v[1], total: v[2], disks: v[3], vms: v[4], nics: v[5] })
    }

    /// Create a VM, attach its disks and NICs, load it and start it.
    /// Returns the VM id and the MAC of each NIC.  On failure the VM is
    /// destroyed again.
    pub fn create_vm(&self, o: &VmSpec) -> Result<(u64, Vec<u64>)> {
        let k = parse_kernel(o.kernel)?;
        let mut name = [0u8; CTL_NAME_MAX];
        let n = o.name.len().min(CTL_NAME_MAX - 1);
        name[..n].copy_from_slice(&o.name.as_bytes()[..n]);
        let resp = self.call(CTL_VM_CREATE, &[o.mem, o.vcpus, k.ram_base, 0, o.io_pool], &name, &[])?;
        let vm = resp.vals[0];
        let r = (|| {
            for &(disk, ro, start, sectors) in &o.disks {
                self.call(CTL_BLK_ATTACH, &[vm, disk, ro as u64, start, sectors], &[], &[])?;
            }
            let mut macs = Vec::new();
            for &(sw, mac, flags) in &o.nics {
                macs.push(self.call(CTL_NIC_CREATE, &[vm, sw, mac, flags], &[], &[])?.vals[0]);
            }
            for (gpa, data) in &k.segments {
                load(self, vm, *gpa, data)?;
            }
            let initrd_gpa = k.end_gpa + (1 << 20);
            if !o.initrd.is_empty() {
                load(self, vm, initrd_gpa, o.initrd)?;
            }
            let mut cmdline = o.cmdline.as_bytes().to_vec();
            cmdline.truncate(CTL_DATA_MAX - 1);
            self.call(CTL_VM_START, &[vm, k.entry, k.end_gpa, initrd_gpa, o.initrd.len() as u64], &cmdline, &[])?;
            Ok((vm, macs))
        })();
        if r.is_err() {
            let _ = self.call(CTL_VM_DESTROY, &[vm], &[], &[]);
        }
        r
    }

    pub fn vm_list(&self) -> Result<Vec<Vm>> {
        let mut out = Vec::new();
        let mut first = 0u64;
        loop {
            let r = self.call(CTL_VM_LIST, &[first], &[], &[])?;
            let n = r.vals[0] as usize;
            if n == 0 {
                return Ok(out);
            }
            for i in 0..n {
                let vm: CtlVm = unsafe {
                    std::ptr::read_unaligned(r.data.as_ptr().add(i * std::mem::size_of::<CtlVm>()) as *const CtlVm)
                };
                let name_len = vm.name.iter().position(|&c| c == 0).unwrap_or(CTL_NAME_MAX);
                out.push(Vm {
                    id: vm.id as u64,
                    state: vm.state,
                    vcpus: vm.vcpus,
                    ram: vm.ram,
                    name: String::from_utf8_lossy(&vm.name[..name_len]).into_owned(),
                });
                first = vm.id as u64 + 1;
            }
        }
    }

    pub fn vm_destroy(&self, vm: u64) -> Result<()> {
        self.call(CTL_VM_DESTROY, &[vm], &[], &[]).map(|_| ())
    }

    /// Ask the guest to power off or reboot cleanly (POWER_OFF, POWER_REBOOT).
    pub fn vm_shutdown(&self, vm: u64, how: u64) -> Result<()> {
        self.call(CTL_VM_SHUTDOWN, &[vm, how], &[], &[]).map(|_| ())
    }

    /// Cap each vCPU at `cap` percent of a core (0: no cap).
    pub fn vm_set_sched(&self, vm: u64, cap: u64) -> Result<()> {
        self.call(CTL_VM_SET_SCHED, &[vm, cap], &[], &[]).map(|_| ())
    }

    /// (CPU used over the last 100 ms in permille of a core, cap in percent).
    pub fn vm_stats(&self, vm: u64) -> Result<(u64, u64)> {
        let r = self.call(CTL_VM_STATS, &[vm], &[], &[])?;
        Ok((r.vals[0], r.vals[1]))
    }

    /// Console output from offset `off`: (next offset, bytes, bytes lost).
    pub fn console_read(&self, vm: u64, off: u64) -> Result<(u64, Vec<u8>, u64)> {
        let r = self.call(CTL_CONSOLE_READ, &[vm, off], &[], &[])?;
        Ok((r.vals[0], r.data[..r.vals[1] as usize].to_vec(), r.vals[2]))
    }

    pub fn switch_create(&self) -> Result<u64> {
        Ok(self.call(CTL_SWITCH_CREATE, &[], &[], &[])?.vals[0])
    }

    /// The ports of a switch and its uplink NIC, if any.
    pub fn switch_info(&self, sw: u64) -> Result<(Vec<CtlPort>, Option<u64>)> {
        let r = self.call(CTL_SWITCH_INFO, &[sw], &[], &[])?;
        let ports = (0..r.vals[0] as usize)
            .map(|i| unsafe {
                std::ptr::read_unaligned(r.data.as_ptr().add(i * std::mem::size_of::<CtlPort>()) as *const CtlPort)
            })
            .collect();
        Ok((ports, (r.vals[1] != u64::MAX).then_some(r.vals[1])))
    }
}
