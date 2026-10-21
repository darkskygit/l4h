//! The ACPI start method (AMD fTPM; TPM2 start methods 2 and 8): each
//! command starts with the TPM device's _DSM, firmware code in AML, which
//! we run with the `acpi` crate's interpreter.  Running it here rather than
//! in root confines what a firmware bug or an interpreter panic can harm
//! to attestation (root then revokes our endpoint).  Accesses outside our
//! own TPM ranges go to root (ROOT_CALL_HW), which refuses RAM and
//! hardware in use.

use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;
use core::ptr::NonNull;
use core::str::FromStr;

use acpi::aml::namespace::{AmlName, NamespaceLevelKind};
use acpi::aml::object::{Object, WrappedObject};
use acpi::aml::{AmlError, NoSendInterpreter};
use acpi::registers::FixedRegisters;
use acpi::sdt::fadt::Fadt;
use acpi::{Handle, PciAddress, RawPhysicalMapping};

use l4h_rt::host::trust::{self, TrustdConfig};
use l4h_rt::{cap, host, sel4};
use sel4::cap_type;

/// The start function of the CRB _DSM (TCG PC Client PTP): its UUID,
/// 6bbf6cab-5463-4714-b7cd-f0203c0368d4, as ToUUID lays it out.
const START_UUID: [u8; 16] = [
    0xab, 0x6c, 0xbf, 0x6b, 0x63, 0x54, 0x14, 0x47, 0xb7, 0xcd, 0xf0, 0x20, 0x3c, 0x03, 0x68, 0xd4,
];
const START_REVISION: u64 = 1;
const START_FUNCTION: u64 = 1;

/// Hardware for the interpreter: our TPM ranges directly, the rest through
/// root.
#[derive(Clone)]
pub struct Hw {
    /// (physical address, size, VA) of each range we have mapped.
    ranges: Vec<(usize, usize, usize)>,
    hz: u64,
    mutexes: Arc<Cell<u32>>,
}

/// What the interpreter maps that is none of our ranges (the PM1 blocks,
/// in system memory on some machines): it only touches them for the
/// firmware's global lock, which we do not take.
static mut SCRATCH: [u8; 4096] = [0; 4096];

impl Hw {
    fn local(&self, paddr: usize, width: usize) -> Option<usize> {
        self.ranges.iter().find(|&&(p, n, _)| p <= paddr && paddr + width <= p + n).map(|&(p, _, va)| va + (paddr - p))
    }

    /// One access through root.
    fn root(&self, op: u64, addr: u64, off: u64, width: usize, value: u64) -> u64 {
        for (i, v) in [op, addr, off, width as u64, value].into_iter().enumerate() {
            sel4::with_ipc_buffer_mut(|b| b.msg_regs_mut()[i] = v);
        }
        cap::<cap_type::Endpoint>(host::SLOT_ROOT_EP).call(sel4::MessageInfo::new(host::ROOT_CALL_HW, 0, 0, 5));
        sel4::with_ipc_buffer(|b| b.msg_regs()[0])
    }

    fn read(&self, paddr: usize, width: usize) -> u64 {
        match self.local(paddr, width) {
            Some(va) => unsafe {
                match width {
                    1 => (va as *const u8).read_volatile() as u64,
                    2 => (va as *const u16).read_volatile() as u64,
                    4 => (va as *const u32).read_volatile() as u64,
                    _ => (va as *const u64).read_volatile(),
                }
            },
            None => self.root(trust::HW_MEM_READ, paddr as u64, 0, width, 0),
        }
    }

    fn write(&self, paddr: usize, width: usize, v: u64) {
        match self.local(paddr, width) {
            Some(va) => unsafe {
                match width {
                    1 => (va as *mut u8).write_volatile(v as u8),
                    2 => (va as *mut u16).write_volatile(v as u16),
                    4 => (va as *mut u32).write_volatile(v as u32),
                    _ => (va as *mut u64).write_volatile(v),
                }
            },
            None => {
                self.root(trust::HW_MEM_WRITE, paddr as u64, 0, width, v);
            }
        }
    }

    fn pci(&self, op: u64, a: PciAddress, off: u16, width: usize, v: u64) -> u64 {
        if a.segment() != 0 {
            return u64::MAX;
        }
        let f = (a.bus() as u64) << 8 | (a.device() as u64) << 3 | a.function() as u64;
        self.root(op, f, off as u64, width, v)
    }

    fn spin_ns(&self, ns: u64) {
        let end = host::ticks::now() + (ns as u128 * self.hz as u128 / 1_000_000_000) as u64;
        while host::ticks::now() < end {
            core::hint::spin_loop();
        }
    }
}

impl acpi::Handler for Hw {
    unsafe fn map_physical_region<T>(&self, paddr: usize, size: usize) -> RawPhysicalMapping<T> {
        let va = self.local(paddr, size).unwrap_or(core::ptr::addr_of_mut!(SCRATCH) as usize);
        unsafe { RawPhysicalMapping::new(paddr, NonNull::new(va as *mut T).unwrap(), size, size) }
    }

    unsafe fn unmap_physical_region<T>(&self, _region: RawPhysicalMapping<T>) {}

    fn read_u8(&self, a: usize) -> u8 {
        self.read(a, 1) as u8
    }
    fn read_u16(&self, a: usize) -> u16 {
        self.read(a, 2) as u16
    }
    fn read_u32(&self, a: usize) -> u32 {
        self.read(a, 4) as u32
    }
    fn read_u64(&self, a: usize) -> u64 {
        self.read(a, 8)
    }
    fn write_u8(&self, a: usize, v: u8) {
        self.write(a, 1, v as u64)
    }
    fn write_u16(&self, a: usize, v: u16) {
        self.write(a, 2, v as u64)
    }
    fn write_u32(&self, a: usize, v: u32) {
        self.write(a, 4, v as u64)
    }
    fn write_u64(&self, a: usize, v: u64) {
        self.write(a, 8, v)
    }
    fn read_io_u8(&self, port: u16) -> u8 {
        self.root(trust::HW_IO_READ, port as u64, 0, 1, 0) as u8
    }
    fn read_io_u16(&self, port: u16) -> u16 {
        self.root(trust::HW_IO_READ, port as u64, 0, 2, 0) as u16
    }
    fn read_io_u32(&self, port: u16) -> u32 {
        self.root(trust::HW_IO_READ, port as u64, 0, 4, 0) as u32
    }
    fn write_io_u8(&self, port: u16, v: u8) {
        self.root(trust::HW_IO_WRITE, port as u64, 0, 1, v as u64);
    }
    fn write_io_u16(&self, port: u16, v: u16) {
        self.root(trust::HW_IO_WRITE, port as u64, 0, 2, v as u64);
    }
    fn write_io_u32(&self, port: u16, v: u32) {
        self.root(trust::HW_IO_WRITE, port as u64, 0, 4, v as u64);
    }
    fn read_pci_u8(&self, a: PciAddress, off: u16) -> u8 {
        self.pci(trust::HW_PCI_READ, a, off, 1, 0) as u8
    }
    fn read_pci_u16(&self, a: PciAddress, off: u16) -> u16 {
        self.pci(trust::HW_PCI_READ, a, off, 2, 0) as u16
    }
    fn read_pci_u32(&self, a: PciAddress, off: u16) -> u32 {
        self.pci(trust::HW_PCI_READ, a, off, 4, 0) as u32
    }
    fn write_pci_u8(&self, a: PciAddress, off: u16, v: u8) {
        self.pci(trust::HW_PCI_WRITE, a, off, 1, v as u64);
    }
    fn write_pci_u16(&self, a: PciAddress, off: u16, v: u16) {
        self.pci(trust::HW_PCI_WRITE, a, off, 2, v as u64);
    }
    fn write_pci_u32(&self, a: PciAddress, off: u16, v: u32) {
        self.pci(trust::HW_PCI_WRITE, a, off, 4, v as u64);
    }
    fn nanos_since_boot(&self) -> u64 {
        (host::ticks::now() as u128 * 1_000_000_000 / self.hz.max(1) as u128) as u64
    }
    fn stall(&self, us: u64) {
        self.spin_ns(us * 1000);
    }
    fn sleep(&self, ms: u64) {
        self.spin_ns(ms * 1_000_000);
    }
    // One thread: mutexes are always free.
    fn create_mutex(&self) -> Handle {
        self.mutexes.set(self.mutexes.get() + 1);
        Handle(self.mutexes.get())
    }
    fn acquire(&self, _mutex: Handle, _timeout: u16) -> Result<(), AmlError> {
        Ok(())
    }
    fn release(&self, _mutex: Handle) {}
    fn handle_fatal_error(&self, fatal_type: u8, fatal_code: u32, fatal_arg: u64) {
        sel4::debug_println!("l4h-trustd: AML Fatal ({:#x}, {:#x}, {:#x})", fatal_type, fatal_code, fatal_arg);
    }
}

/// Warnings and errors of the interpreter, on the serial port.
struct Log;

impl log::Log for Log {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            sel4::debug_println!("l4h-trustd: aml: {}", r.args());
        }
    }
    fn flush(&self) {}
}

static LOG: Log = Log;

/// The TPM device's _DSM, ready to start commands.
pub struct Start {
    interp: NoSendInterpreter<Hw>,
    dsm: AmlName,
}

fn dsm_args(function: u64) -> Vec<WrappedObject> {
    vec![
        Object::Buffer(START_UUID.to_vec()).wrap(),
        Object::Integer(START_REVISION).wrap(),
        Object::Integer(function).wrap(),
        Object::Package(Vec::new()).wrap(),
    ]
}

fn aml_err(what: &'static str) -> impl FnOnce(AmlError) -> &'static str {
    move |e| {
        sel4::debug_println!("l4h-trustd: {}: {:?}", what, e);
        what
    }
}

/// Is `id` (a _HID or _CID) the TPM's, MSFT0101?
fn is_tpm(id: &Object) -> bool {
    match id {
        Object::String(s) => s.as_str() == "MSFT0101",
        Object::Package(p) => p.iter().any(|o| is_tpm(o)),
        _ => false,
    }
}

impl Start {
    /// Load the tables root gave us and find the TPM device whose _DSM
    /// has the start function.
    pub fn new(cfg: &TrustdConfig, hz: u64) -> Result<Start, &'static str> {
        let _ = log::set_logger(&LOG).map(|()| log::set_max_level(log::LevelFilter::Warn));
        let all: &'static [u8] = unsafe { core::slice::from_raw_parts(trust::AML_VA as *const u8, cfg.aml_size as usize) };
        let mut tables = Vec::new();
        let mut at = 0;
        while at + 36 <= all.len() {
            let len = u32::from_le_bytes(all[at + 4..at + 8].try_into().unwrap()) as usize;
            if len < 36 || at + len > all.len() {
                return Err("bad ACPI table");
            }
            tables.push(&all[at..at + len]);
            at += len;
        }
        let [fadt, dsdt, ssdts @ ..] = tables.as_slice() else { return Err("no FADT or DSDT") };
        if &fadt[..4] != b"FACP" || &dsdt[..4] != b"DSDT" {
            return Err("no FADT or DSDT");
        }
        let ranges = cfg
            .ranges
            .iter()
            .enumerate()
            .filter(|(_, r)| r.size != 0)
            .map(|(i, r)| (r.paddr as usize, r.size as usize, trust::TPM_VA + i * trust::RANGE_STRIDE))
            .collect();
        let hw = Hw { ranges, hz, mutexes: Arc::new(Cell::new(0)) };
        // The FADT as the crate reads it (a packed struct).
        let fadt: &Fadt = unsafe { &*(fadt.as_ptr() as *const Fadt) };
        let registers = FixedRegisters::new(fadt, hw.clone()).map_err(|_| "FADT PM1 registers")?;
        let interp = NoSendInterpreter::new(hw, dsdt[8], Arc::new(registers), None);
        interp.load_table(&dsdt[36..]).map_err(aml_err("loading the DSDT"))?;
        for t in ssdts.iter().filter(|t| &t[..4] == b"SSDT") {
            interp.load_table(&t[36..]).map_err(aml_err("loading an SSDT"))?;
        }

        let mut devices = Vec::new();
        interp
            .namespace
            .lock()
            .clone()
            .traverse(|path, level| {
                if matches!(level.kind, NamespaceLevelKind::Device) {
                    devices.push(path.clone());
                }
                Ok(true)
            })
            .map_err(aml_err("walking the namespace"))?;
        let name = |s: &str, dev: &AmlName| AmlName::from_str(s).and_then(|n| n.resolve(dev));
        for dev in devices {
            let id = |s| name(s, &dev).and_then(|n| interp.evaluate_if_present(n, vec![]));
            let tpm = |s| id(s).ok().flatten().is_some_and(|o| is_tpm(&o));
            if !tpm("_HID") && !tpm("_CID") {
                continue;
            }
            // Its _INI, and its parents' (only these: other devices' are
            // none of our business).
            let mut path = dev.clone();
            let mut scopes = vec![dev.clone()];
            while let Ok(p) = path.parent() {
                if p.to_string() == "\\" || scopes.contains(&p) {
                    break;
                }
                scopes.push(p.clone());
                path = p;
            }
            for s in scopes.iter().rev() {
                if let Ok(ini) = name("_INI", s) {
                    interp.evaluate_if_present(ini, vec![]).map_err(aml_err("_INI"))?;
                }
            }
            let dsm = name("_DSM", &dev).map_err(aml_err("_DSM name"))?;
            // Function 0: the functions it has, a bit each.
            let supported = interp.evaluate_if_present(dsm.clone(), dsm_args(0)).ok().flatten();
            let has_start = supported.as_ref().and_then(|o| o.as_buffer().ok()).is_some_and(|b| b.first().is_some_and(|f| f & (1 << START_FUNCTION) != 0));
            if has_start {
                return Ok(Start { interp, dsm });
            }
        }
        Err("no MSFT0101 device with the CRB start _DSM")
    }

    /// Start the command in the CRB buffer.
    pub fn start(&self) -> Result<(), &'static str> {
        let r = self.interp.evaluate(self.dsm.clone(), dsm_args(START_FUNCTION)).map_err(aml_err("_DSM start"))?;
        match r.as_integer() {
            Ok(0) => Ok(()),
            _ => Err("_DSM start failed"),
        }
    }
}
