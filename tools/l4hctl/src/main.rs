//! l4hctl: dom0 client of the l4h control API (DESIGN.md §9.1), on
//! /dev/l4hctl.  The request layouts mirror `include/l4h/abi.h`.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::process::exit;
use std::time::{Duration, Instant};

const CTL_VERSION: u32 = 1;
const CTL_DATA_MAX: usize = 1984;
const CTL_LOAD_MAX: usize = 4 << 20;
const CTL_NAME_MAX: usize = 32;

const CTL_HOST_INFO: u32 = 1;
const CTL_VM_CREATE: u32 = 2;
const CTL_VM_LOAD: u32 = 3;
const CTL_VM_START: u32 = 4;
const CTL_VM_DESTROY: u32 = 5;
const CTL_VM_LIST: u32 = 6;
const CTL_BLK_ATTACH: u32 = 7;
const CTL_CONSOLE_READ: u32 = 8;
const CTL_CONSOLE_WRITE: u32 = 9;
const CTL_DISK_INFO: u32 = 10;
const CTL_SWITCH_CREATE: u32 = 11;
const CTL_SWITCH_ATTACH_UPLINK: u32 = 12;
const CTL_NIC_CREATE: u32 = 13;
const CTL_SWITCH_INFO: u32 = 14;
const CTL_NIC_INFO: u32 = 15;
const NIC_NO_MAC_LOCK: u64 = 1;
const CTL_VM_SET_SCHED: u32 = 16;
const CTL_VM_INFO: u32 = 17;
const CTL_VM_STATS: u32 = 18;
const CTL_VM_SHUTDOWN: u32 = 19;

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlPort {
    vm: u32,
    port: u32,
    mac: u64,
    rx_frames: u64,
    tx_frames: u64,
    drops: u64,
}

fn fmt_mac(m: u64) -> String {
    let b = m.to_be_bytes();
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[2], b[3], b[4], b[5], b[6], b[7])
}

fn parse_mac(s: &str) -> Result<u64> {
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
struct Req {
    op: u32,
    version: u32,
    args: [u64; 6],
    data: [u8; CTL_DATA_MAX],
}

#[repr(C)]
struct Resp {
    status: i64,
    vals: [u64; 6],
    data: [u8; CTL_DATA_MAX],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlVm {
    id: u32,
    state: u32,
    vcpus: u32,
    reserved: u32,
    ram: u64,
    name: [u8; CTL_NAME_MAX],
}

#[repr(C)]
struct Call {
    req: u64,
    resp: u64,
    buf: u64,
    len: u64,
}

// _IOWR('L', 1, struct l4hctl_call)
const L4HCTL_IOC_CALL: u64 = (3 << 30) | ((std::mem::size_of::<Call>() as u64) << 16) | ((b'L' as u64) << 8) | 1;

unsafe extern "C" {
    fn ioctl(fd: i32, req: u64, ...) -> i32;
}

struct Ctl {
    dev: File,
}

#[derive(Debug)]
struct Error(String);

type Result<T> = std::result::Result<T, Error>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

fn errno_name(e: i64) -> &'static str {
    match e {
        1 => "operation not permitted",
        2 => "no such VM or device",
        12 => "out of memory",
        16 => "busy",
        22 => "invalid argument",
        28 => "no space for devices",
        38 => "not supported",
        _ => "error",
    }
}

impl Ctl {
    fn open() -> Result<Self> {
        File::options()
            .read(true)
            .write(true)
            .open("/dev/l4hctl")
            .map(|dev| Ctl { dev })
            .map_err(|e| Error(format!("/dev/l4hctl: {e} (not dom0?)")))
    }

    fn call(&self, op: u32, args: &[u64], data: &[u8], buf: &[u8]) -> Result<Box<Resp>> {
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
struct Kernel {
    entry: u64,
    ram_base: u64,
    end_gpa: u64,
    /// (guest-physical address, bytes)
    segments: Vec<(u64, Vec<u8>)>,
}

fn rd<const N: usize>(b: &[u8], off: usize) -> Result<[u8; N]> {
    b.get(off..off + N).map(|s| s.try_into().unwrap()).ok_or(Error("truncated ELF".into()))
}

fn parse_kernel(b: &[u8]) -> Result<Kernel> {
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

fn load(ctl: &Ctl, vm: u64, gpa: u64, data: &[u8]) -> Result<()> {
    for (i, chunk) in data.chunks(CTL_LOAD_MAX).enumerate() {
        ctl.call(CTL_VM_LOAD, &[vm, gpa + (i * CTL_LOAD_MAX) as u64], &[], chunk)?;
    }
    Ok(())
}

fn read_file(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error(format!("{path}: {e}")))
}

fn parse_num(s: &str) -> Result<u64> {
    s.parse().map_err(|_| Error(format!("bad number: {s}")))
}

/// A size in MiB, as bytes.
fn parse_mib(s: &str) -> Result<u64> {
    parse_num(s)?.checked_mul(1 << 20).ok_or(Error(format!("too large: {s} MiB")))
}

struct CreateOpts {
    name: String,
    mem: u64,
    vcpus: u64,
    kernel: String,
    initrd: Option<String>,
    cmdline: String,
    /// (disk, read-only, first sector, sectors)
    disks: Vec<(u64, bool, u64, u64)>,
    /// seL4 object quota in bytes (0: default).
    quota: u64,
    /// (switch, MAC or 0, flags)
    nics: Vec<(u64, u64, u64)>,
}

fn cmd_create(ctl: &Ctl, o: &CreateOpts) -> Result<u64> {
    let k = parse_kernel(&read_file(&o.kernel)?)?;
    let initrd = match &o.initrd {
        Some(p) => read_file(p)?,
        None => Vec::new(),
    };
    let mut name = [0u8; CTL_NAME_MAX];
    let n = o.name.len().min(CTL_NAME_MAX - 1);
    name[..n].copy_from_slice(&o.name.as_bytes()[..n]);
    let resp = ctl.call(CTL_VM_CREATE, &[o.mem, o.vcpus, k.ram_base, o.quota], &name, &[])?;
    let vm = resp.vals[0];
    let r = (|| {
        for &(disk, ro, start, sectors) in &o.disks {
            ctl.call(CTL_BLK_ATTACH, &[vm, disk, ro as u64, start, sectors], &[], &[])?;
        }
        for &(sw, mac, flags) in &o.nics {
            let r = ctl.call(CTL_NIC_CREATE, &[vm, sw, mac, flags], &[], &[])?;
            eprintln!("vm {vm}: nic on switch {sw}, mac {}", fmt_mac(r.vals[0]));
        }
        for (gpa, data) in &k.segments {
            load(ctl, vm, *gpa, data)?;
        }
        let initrd_gpa = k.end_gpa + (1 << 20);
        if !initrd.is_empty() {
            load(ctl, vm, initrd_gpa, &initrd)?;
        }
        let mut cmdline = o.cmdline.as_bytes().to_vec();
        cmdline.truncate(CTL_DATA_MAX - 1);
        ctl.call(CTL_VM_START, &[vm, k.entry, k.end_gpa, initrd_gpa, initrd.len() as u64], &cmdline, &[])?;
        Ok(vm)
    })();
    if r.is_err() {
        let _ = ctl.call(CTL_VM_DESTROY, &[vm], &[], &[]);
    }
    r
}

fn cmd_console(ctl: &Ctl, vm: u64, follow: bool, until: Option<&str>, timeout: Option<Duration>) -> Result<bool> {
    let start = Instant::now();
    let mut off = 0u64;
    let mut seen = Vec::new();
    let out = io::stdout();
    loop {
        let r = ctl.call(CTL_CONSOLE_READ, &[vm, off], &[], &[])?;
        let n = r.vals[1] as usize;
        if r.vals[2] != 0 {
            eprintln!("[l4hctl: {} console bytes lost]", r.vals[2]);
        }
        off = r.vals[0];
        let mut o = out.lock();
        let _ = o.write_all(&r.data[..n]);
        let _ = o.flush();
        if let Some(u) = until {
            seen.extend_from_slice(&r.data[..n]);
            if seen.windows(u.len()).any(|w| w == u.as_bytes()) {
                return Ok(true);
            }
            if seen.len() > 2 * u.len() + 4096 {
                seen.drain(..seen.len() - u.len());
            }
        }
        if n == 0 {
            if !follow && until.is_none() {
                return Ok(true);
            }
            if timeout.is_some_and(|t| start.elapsed() > t) {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

const USAGE: &str = "usage: l4hctl <command>
  info                         host resources
  disks                        host disks and their owners
  list                         VMs
  create NAME --kernel F [--initrd F] [--mem MB] [--vcpus N]
         [--cmdline S] [--disk I[,ro][,start=SECTOR][,sectors=N]]...
                               create and start a VM; prints its id
         [--nic SWITCH[,mac=MAC][,nolock]]...
  shutdown VM [--reboot]       ask the VM to shut down (or reboot) cleanly
  destroy VM                   stop a VM and free its resources
  sched VM CAP                 cap each vCPU of VM at CAP% of a core (0: none)
  cores VM                     host core of each vCPU of VM
  top                          CPU use of each VM
  nics                         host NICs
  switch-create                create an L2 switch; prints its id
  uplink SWITCH NIC            make host NIC the uplink of SWITCH
  switch SWITCH                ports and counters of SWITCH
  console VM [--follow] [--until STR] [--timeout SECS]
                               print the VM's console output
  send VM [--wait PAT [--timeout SECS]] TEXT
                               type TEXT (plus newline) on the VM's console;
                               with --wait, print the time until PAT appears";

fn run(args: &[String]) -> Result<i32> {
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    let ctl = Ctl::open()?;
    match cmd.as_str() {
        "info" => {
            let r = ctl.call(CTL_HOST_INFO, &[], &[], &[])?;
            println!("cores:  {}", r.vals[0]);
            println!("memory: {} MiB free of {} MiB", r.vals[1] >> 20, r.vals[2] >> 20);
            println!("disks:  {}", r.vals[3]);
            println!("vms:    {}", r.vals[4]);
        }
        "disks" => {
            let n = ctl.call(CTL_HOST_INFO, &[], &[], &[])?.vals[3];
            for d in 0..n {
                let r = ctl.call(CTL_DISK_INFO, &[d], &[], &[])?;
                let bdf = r.vals[0];
                let owner = if r.vals[1] == u64::MAX { "-".to_string() } else { format!("vm {}", r.vals[1]) };
                let kind = if r.vals[3] == 1 { "nvme" } else { "virtio-blk" };
                println!("disk {d}: pci {:02x}:{:02x}.{} {kind}, owner {owner}", bdf >> 8, (bdf >> 3) & 0x1f, bdf & 7);
            }
        }
        "list" => {
            println!("{:>3} {:<16} {:>5} {:>8} {}", "ID", "NAME", "VCPUS", "MEM", "STATE");
            let mut first = 0u64;
            loop {
                let r = ctl.call(CTL_VM_LIST, &[first], &[], &[])?;
                let n = r.vals[0] as usize;
                if n == 0 {
                    break;
                }
                for i in 0..n {
                    let vm: CtlVm = unsafe {
                        std::ptr::read_unaligned(r.data.as_ptr().add(i * std::mem::size_of::<CtlVm>()) as *const CtlVm)
                    };
                    let name_len = vm.name.iter().position(|&c| c == 0).unwrap_or(CTL_NAME_MAX);
                    let state = match vm.state {
                        1 => "created",
                        2 => "running",
                        3 => "stopped",
                        _ => "?",
                    };
                    println!(
                        "{:>3} {:<16} {:>5} {:>7}M {}",
                        vm.id,
                        String::from_utf8_lossy(&vm.name[..name_len]),
                        vm.vcpus,
                        vm.ram >> 20,
                        state
                    );
                    first = vm.id as u64 + 1;
                }
            }
        }
        "create" => {
            let name = args.get(1).ok_or(Error("create: missing NAME".into()))?.clone();
            let mut o = CreateOpts {
                name,
                mem: 128 << 20,
                vcpus: 1,
                kernel: String::new(),
                initrd: None,
                cmdline: "console=hvc0".into(),
                disks: Vec::new(),
                quota: 0,
                nics: Vec::new(),
            };
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                let mut val = || it.next().cloned().ok_or(Error(format!("{a}: missing value")));
                match a.as_str() {
                    "--kernel" => o.kernel = val()?,
                    "--initrd" => o.initrd = Some(val()?),
                    "--mem" => o.mem = parse_mib(&val()?)?,
                    "--vcpus" => o.vcpus = parse_num(&val()?)?,
                    "--quota" => o.quota = parse_mib(&val()?)?,
                    "--cmdline" => o.cmdline = val()?,
                    "--disk" => {
                        // DISK[,ro][,start=SECTOR][,sectors=N] (or DISK:ro)
                        let v = val()?.replace(":ro", ",ro");
                        let mut it = v.split(',');
                        let d = parse_num(it.next().unwrap())?;
                        let (mut ro, mut start, mut sectors) = (false, 0, 0);
                        for opt in it {
                            if opt == "ro" {
                                ro = true;
                            } else if let Some(n) = opt.strip_prefix("start=") {
                                start = parse_num(n)?;
                            } else if let Some(n) = opt.strip_prefix("sectors=") {
                                sectors = parse_num(n)?;
                            } else {
                                return err(format!("--disk: unknown option {opt}"));
                            }
                        }
                        o.disks.push((d, ro, start, sectors));
                    }
                    "--nic" => {
                        // SWITCH[,mac=MAC][,nolock]
                        let v = val()?;
                        let mut it = v.split(',');
                        let sw = parse_num(it.next().unwrap())?;
                        let (mut mac, mut flags) = (0, 0);
                        for opt in it {
                            if let Some(m) = opt.strip_prefix("mac=") {
                                mac = parse_mac(m)?;
                            } else if opt == "nolock" {
                                flags |= NIC_NO_MAC_LOCK;
                            } else {
                                return err(format!("--nic: unknown option {opt}"));
                            }
                        }
                        o.nics.push((sw, mac, flags));
                    }
                    _ => return err(format!("create: unknown option {a}")),
                }
            }
            if o.kernel.is_empty() {
                return err("create: --kernel is required");
            }
            println!("{}", cmd_create(&ctl, &o)?);
        }
        "sched" => {
            let vm = parse_num(args.get(1).ok_or(Error("sched: missing VM".into()))?)?;
            let cap = parse_num(args.get(2).ok_or(Error("sched: missing CAP".into()))?)?;
            ctl.call(CTL_VM_SET_SCHED, &[vm, cap], &[], &[])?;
        }
        "top" => {
            // CPU use of every VM over the last balancer period.
            let mut first = 0u64;
            println!("{:>3} {:<16} {:>6} {:>5}", "ID", "NAME", "CPU%", "CAP%");
            loop {
                let r = ctl.call(CTL_VM_LIST, &[first], &[], &[])?;
                let n = r.vals[0] as usize;
                if n == 0 {
                    break;
                }
                for i in 0..n {
                    let vm: CtlVm = unsafe {
                        std::ptr::read_unaligned(r.data.as_ptr().add(i * std::mem::size_of::<CtlVm>()) as *const CtlVm)
                    };
                    let s = ctl.call(CTL_VM_STATS, &[vm.id as u64], &[], &[])?;
                    let name_len = vm.name.iter().position(|&c| c == 0).unwrap_or(CTL_NAME_MAX);
                    println!(
                        "{:>3} {:<16} {:>6.1} {:>5}",
                        vm.id,
                        String::from_utf8_lossy(&vm.name[..name_len]),
                        s.vals[0] as f64 / 10.0,
                        s.vals[1]
                    );
                    first = vm.id as u64 + 1;
                }
            }
        }
        "cores" => {
            let vm = parse_num(args.get(1).ok_or(Error("cores: missing VM".into()))?)?;
            let r = ctl.call(CTL_VM_INFO, &[vm], &[], &[])?;
            let n = ctl.call(CTL_VM_LIST, &[vm], &[], &[])?;
            let vcpus = if n.vals[0] > 0 {
                unsafe { std::ptr::read_unaligned(n.data.as_ptr() as *const CtlVm) }.vcpus as usize
            } else {
                0
            };
            let cores: Vec<String> = r.vals[..vcpus.min(6)].iter().map(|c| c.to_string()).collect();
            println!("{}", cores.join(" "));
        }
        "nics" => {
            let n = ctl.call(CTL_HOST_INFO, &[], &[], &[])?.vals[5];
            for i in 0..n {
                let r = ctl.call(CTL_NIC_INFO, &[i], &[], &[])?;
                let bdf = r.vals[0];
                let sw = if r.vals[1] == u64::MAX { "-".to_string() } else { format!("switch {}", r.vals[1]) };
                println!("nic {i}: pci {:02x}:{:02x}.{} virtio-net, uplink of {sw}", bdf >> 8, (bdf >> 3) & 0x1f, bdf & 7);
            }
        }
        "switch-create" => {
            println!("{}", ctl.call(CTL_SWITCH_CREATE, &[], &[], &[])?.vals[0]);
        }
        "uplink" => {
            let sw = parse_num(args.get(1).ok_or(Error("uplink: missing SWITCH".into()))?)?;
            let nic = parse_num(args.get(2).ok_or(Error("uplink: missing NIC".into()))?)?;
            ctl.call(CTL_SWITCH_ATTACH_UPLINK, &[sw, nic], &[], &[])?;
        }
        "switch" => {
            let sw = parse_num(args.get(1).ok_or(Error("switch: missing SWITCH".into()))?)?;
            let r = ctl.call(CTL_SWITCH_INFO, &[sw], &[], &[])?;
            if r.vals[1] != u64::MAX {
                println!("switch {sw}: uplink nic {}", r.vals[1]);
            } else {
                println!("switch {sw}: no uplink");
            }
            println!("{:>4} {:>3} {:<17} {:>8} {:>8} {:>6}", "PORT", "VM", "MAC", "RX", "TX", "DROPS");
            for i in 0..r.vals[0] as usize {
                let p: CtlPort = unsafe {
                    std::ptr::read_unaligned(r.data.as_ptr().add(i * std::mem::size_of::<CtlPort>()) as *const CtlPort)
                };
                println!("{:>4} {:>3} {:<17} {:>8} {:>8} {:>6}", p.port, p.vm, fmt_mac(p.mac), p.rx_frames, p.tx_frames, p.drops);
            }
        }
        "shutdown" => {
            let vm = parse_num(args.get(1).ok_or(Error("shutdown: missing VM".into()))?)?;
            let how = if args.get(2).map(|s| s.as_str()) == Some("--reboot") { 1 } else { 0 };
            ctl.call(CTL_VM_SHUTDOWN, &[vm, how], &[], &[])?;
        }
        "destroy" => {
            let vm = parse_num(args.get(1).ok_or(Error("destroy: missing VM".into()))?)?;
            ctl.call(CTL_VM_DESTROY, &[vm], &[], &[])?;
        }
        "console" => {
            let vm = parse_num(args.get(1).ok_or(Error("console: missing VM".into()))?)?;
            let (mut follow, mut until, mut timeout) = (false, None, None);
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--follow" | "-f" => follow = true,
                    "--until" => until = Some(it.next().filter(|s| !s.is_empty()).cloned().ok_or(Error("--until: missing pattern".into()))?),
                    "--timeout" => {
                        let s = parse_num(it.next().ok_or(Error("--timeout: missing value".into()))?)?;
                        timeout = Some(Duration::from_secs(s));
                    }
                    _ => return err(format!("console: unknown option {a}")),
                }
            }
            if !cmd_console(&ctl, vm, follow, until.as_deref(), timeout)? {
                eprintln!("l4hctl: timed out");
                return Ok(1);
            }
        }
        "send" => {
            let vm = parse_num(args.get(1).ok_or(Error("send: missing VM".into()))?)?;
            // send VM [--wait PATTERN [--timeout SECS]] TEXT...
            let mut rest = &args[2..];
            let (mut wait, mut timeout) = (None, Duration::from_secs(30));
            loop {
                match rest.first().map(|s| s.as_str()) {
                    Some("--wait") if rest.len() > 1 => {
                        if rest[1].is_empty() {
                            return err("--wait: empty pattern");
                        }
                        wait = Some(rest[1].clone());
                        rest = &rest[2..];
                    }
                    Some("--timeout") if rest.len() > 1 => {
                        timeout = Duration::from_secs(parse_num(&rest[1])?);
                        rest = &rest[2..];
                    }
                    _ => break,
                }
            }
            // Skip output so far, so that only the answer is matched.
            let mut con = 0u64;
            if wait.is_some() {
                loop {
                    let r = ctl.call(CTL_CONSOLE_READ, &[vm, con], &[], &[])?;
                    con = r.vals[0];
                    if r.vals[1] == 0 {
                        break;
                    }
                }
            }
            let start = Instant::now();
            let mut text = rest.join(" ").into_bytes();
            text.push(b'\n');
            let mut off = 0;
            while off < text.len() {
                let chunk = &text[off..(off + CTL_DATA_MAX).min(text.len())];
                let r = ctl.call(CTL_CONSOLE_WRITE, &[vm, chunk.len() as u64], chunk, &[])?;
                off += r.vals[0] as usize;
                if r.vals[0] == 0 {
                    // The guest is not reading its console.
                    if start.elapsed() > timeout {
                        eprintln!("l4hctl: timed out writing to the console");
                        return Ok(1);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            if let Some(pat) = wait {
                let mut seen = Vec::new();
                loop {
                    let r = ctl.call(CTL_CONSOLE_READ, &[vm, con], &[], &[])?;
                    con = r.vals[0];
                    seen.extend_from_slice(&r.data[..r.vals[1] as usize]);
                    if seen.windows(pat.len()).any(|w| w == pat.as_bytes()) {
                        println!("{:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
                        break;
                    }
                    if start.elapsed() > timeout {
                        eprintln!("l4hctl: timed out");
                        return Ok(1);
                    }
                    if r.vals[1] == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            return Ok(2);
        }
    }
    Ok(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => exit(code),
        Err(Error(e)) => {
            eprintln!("l4hctl: {e}");
            exit(1);
        }
    }
}
