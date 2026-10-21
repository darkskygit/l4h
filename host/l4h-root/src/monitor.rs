//! A minimal monitor on the serial console (COM1, the GCE serial port, or
//! the serial_port and serial_irq settings, e.g. COM2 for Supermicro
//! serial-over-LAN),
//! for when dom0 is not usable (DESIGN.md §9.1): list, destroy,
//! restart-dom0.  The kernel keeps printing to the same port; we only take
//! its receive side.

use alloc::string::String;

use sel4::cap_type;

use crate::Host;
use crate::mem::Alloc;

/// Badge of the monitor's interrupt on the root task's bound notification.
pub const MONITOR_BADGE: u64 = 1 << 41;

/// seL4 user IRQ vector for the serial port.
#[cfg(target_arch = "x86_64")]
const VECTOR: u64 = crate::pci::VECTOR_MONITOR;

pub struct Monitor {
    port: u64,
    /// The UART's first I/O port (x86).
    #[cfg(target_arch = "x86_64")]
    base: u16,
    irq: sel4::cap::IrqHandler,
    line: String,
}

impl Monitor {
    #[cfg(target_arch = "x86_64")]
    pub fn new(alloc: &mut Alloc, _devmem: &mut crate::devmem::DevMem, ntfn: sel4::cap::Notification, base: u16, irq: u64) -> Option<Self> {
        use sel4::init_thread::slot;
        let abs = |s: usize| slot::CNODE.cap().absolute_cptr(sel4::init_thread::Slot::<cap_type::Null>::from_index(s).cap());
        let port = alloc.slots.alloc();
        slot::IO_PORT_CONTROL.cap().ioport_control_issue(base as u64, base as u64 + 7, &abs(port)).ok()?;
        let irq_slot = alloc.slots.alloc();
        slot::IRQ_CONTROL.cap().irq_control_get_ioapic(0, irq, 0, 0, VECTOR, &abs(irq_slot)).ok()?;
        let irq = sel4::init_thread::Slot::<cap_type::IrqHandler>::from_index(irq_slot).cap();
        let badged = alloc.slots.alloc();
        abs(badged)
            .mint(&slot::CNODE.cap().absolute_cptr(ntfn), sel4::CapRights::write_only(), MONITOR_BADGE)
            .ok()?;
        irq.irq_handler_set_notification(sel4::init_thread::Slot::<cap_type::Notification>::from_index(badged).cap()).ok()?;
        let m = Monitor { port: port as u64, base, irq, line: String::new() };
        // Receive-data interrupt; OUT2 routes UART interrupts to the PIC/IOAPIC.
        m.out(1, 0x01);
        m.out(4, m.inb(4) | 0x08);
        info!("l4h-root: serial monitor on port {:#x} (type 'help')", base);
        Some(m)
    }

    #[cfg(target_arch = "aarch64")]
    pub fn new(alloc: &mut Alloc, devmem: &mut crate::devmem::DevMem, ntfn: sel4::cap::Notification, _base: u16, _irq: u64) -> Option<Self> {
        use sel4::init_thread::slot;
        use sel4::{CapRights, VmAttributes};
        use crate::mem::ObjCtx;
        const UART_VA: usize = crate::layout::UART.base;
        let frames = devmem.frames(&mut alloc.slots, 0x0900_0000, 4096)?;
        crate::vspace::map_frame(&mut ObjCtx::global(alloc), slot::VSPACE.cap(), frames[0],
            UART_VA, CapRights::read_write(), VmAttributes::EXECUTE_NEVER);
        let irq_slot = alloc.slots.alloc();
        let dst = crate::component::abs_of(sel4::init_thread::Slot::<cap_type::Null>::from_index(irq_slot).cap());
        slot::IRQ_CONTROL.cap().irq_control_get_trigger_core(33, false, 0, &dst).ok()?;
        let irq = sel4::init_thread::Slot::<cap_type::IrqHandler>::from_index(irq_slot).cap();
        let badge_slot = alloc.slots.alloc();
        let badge = sel4::init_thread::Slot::<cap_type::Notification>::from_index(badge_slot).cap();
        crate::component::abs_of(badge).mint(&crate::component::abs_of(ntfn), CapRights::write_only(), MONITOR_BADGE).ok()?;
        irq.irq_handler_set_notification(badge).ok()?;
        unsafe {
            ((UART_VA + 0x44) as *mut u32).write_volatile(0x7ff); // clear pending IRQs
            ((UART_VA + 0x38) as *mut u32).write_volatile((1 << 4) | (1 << 6)); // RX, RX timeout
        }
        irq.irq_handler_ack().ok()?;
        info!("l4h-root: serial monitor on PL011 (type 'help')");
        Some(Self { port: UART_VA as u64, irq, line: String::new() })
    }

    #[cfg(target_arch = "x86_64")]
    fn inb(&self, off: u16) -> u8 {
        sel4::with_ipc_buffer_mut(|ipc| ipc.inner_mut().seL4_X86_IOPort_In8(self.port, self.base + off).result as u8)
    }

    #[cfg(target_arch = "x86_64")]
    fn out(&self, off: u16, v: u8) {
        sel4::with_ipc_buffer_mut(|ipc| ipc.inner_mut().seL4_X86_IOPort_Out8(self.port, (self.base + off) as u64, v as u64));
    }

    #[cfg(target_arch = "aarch64")]
    fn inb(&self, off: u16) -> u8 {
        unsafe {
            if off == 5 {
                (((self.port as usize + 0x18) as *const u32).read_volatile() & (1 << 4) == 0) as u8
            } else {
                (self.port as *const u32).read_volatile() as u8
            }
        }
    }

    /// The UART interrupted: collect input, run complete lines.
    pub fn interrupt(h: &mut Host) {
        let Some(mut m) = h.monitor.take() else { return };
        while m.inb(5) & 1 != 0 {
            let c = m.inb(0);
            match c {
                b'\r' | b'\n' => {
                    let line = core::mem::take(&mut m.line);
                    if !line.trim().is_empty() {
                        run(h, line.trim());
                    }
                }
                0x08 | 0x7f => {
                    m.line.pop();
                }
                c if (0x20..0x7f).contains(&c) && m.line.len() < 80 => m.line.push(c as char),
                _ => {}
            }
        }
        #[cfg(target_arch = "aarch64")]
        unsafe { ((m.port + 0x44) as *mut u32).write_volatile(0x7ff); }
        let _ = m.irq.irq_handler_ack();
        h.monitor = Some(m);
    }
}

fn run(h: &mut Host, line: &str) {
    let mut words = line.split_whitespace();
    let cmd = words.next().unwrap_or("");
    match cmd {
        "help" => sel4::debug_println!("monitor: help | list | info | destroy <vm> | restart-dom0"),
        "list" => {
            for g in h.guests.iter().flatten() {
                sel4::debug_println!(
                    "monitor: vm {} '{}': {:?}, {} vCPU(s), {} MiB{}",
                    g.id, g.name, g.state, g.vcpus.len(), g.ram_size() >> 20,
                    if g.dom0 { ", dom0" } else { "" }
                );
            }
        }
        "info" => sel4::debug_println!(
            "monitor: {} cores, {} MiB free of {} MiB ({} MiB reported to the hypervisor), {} disk(s), {} NIC(s)",
            h.info.cores, h.alloc.free_bytes() >> 20, h.total_bytes >> 20, h.alloc.reported_bytes() >> 20,
            h.disks.len(), h.netd.nics.len()
        ),
        "destroy" => match words.next().and_then(|w| w.parse::<usize>().ok()) {
            Some(id) if h.guest(id).is_some_and(|g| !g.dom0) => {
                let _ = h.destroy_guest(id);
            }
            _ => sel4::debug_println!("monitor: destroy <vm> (not dom0; use restart-dom0)"),
        },
        "restart-dom0" => match h.guests.iter().flatten().find(|g| g.dom0).map(|g| g.id) {
            Some(id) => crate::restart_dom0(h, id),
            None => crate::boot_dom0(h),
        },
        _ => sel4::debug_println!("monitor: unknown command '{}' (try 'help')", cmd),
    }
}
