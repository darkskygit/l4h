//! Wall-clock time at boot, from the CMOS real-time clock (x86).  Guests
//! derive their wall clock from it (L4H_HC_GET_TIME) and may correct it
//! with NTP.

#[cfg(target_arch = "x86_64")]
use sel4::cap_type;

use crate::mem::Alloc;

#[cfg(target_arch = "x86_64")]
const CMOS: u16 = 0x70;

/// Nanoseconds since the Unix epoch, read now; 0 if there is no RTC.
#[cfg(target_arch = "x86_64")]
pub fn read_ns(alloc: &mut Alloc, _devmem: &mut crate::devmem::DevMem, _win: &mut crate::window::Window) -> u64 {
    use sel4::init_thread::slot;
    let s = alloc.slots.alloc();
    let dst = slot::CNODE.cap().absolute_cptr(sel4::init_thread::Slot::<cap_type::Null>::from_index(s).cap());
    if slot::IO_PORT_CONTROL.cap().ioport_control_issue(CMOS as u64, CMOS as u64 + 1, &dst).is_err() {
        return 0;
    }
    let port = s as u64;
    let t = sample(port);
    let _ = dst.delete();
    alloc.slots.free(s);
    let Some([sec, min, hour_raw, day, mon, year, century, b]) = t else {
        sel4::debug_println!("l4h-root: cannot read the RTC");
        return 0;
    };
    let bcd = |v: u8| if b & 0x04 == 0 { (v & 0x0f) + (v >> 4) * 10 } else { v };
    let pm = b & 0x02 == 0 && hour_raw & 0x80 != 0;
    let mut hour = bcd(hour_raw & 0x7f) as u64;
    if b & 0x02 == 0 {
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let century = match bcd(century) {
        19..=21 => bcd(century) as i64,
        _ => 20,
    };
    let y = century * 100 + bcd(year) as i64;
    let days = days_from_civil(y, bcd(mon) as i64, bcd(day) as i64);
    if days < 0 {
        return 0;
    }
    let secs = days as u64 * 86400 + hour * 3600 + bcd(min) as u64 * 60 + bcd(sec) as u64;
    secs * 1_000_000_000
}

/// QEMU virt's PL031: a 32-bit seconds counter at register DR.
#[cfg(target_arch = "aarch64")]
pub fn read_ns(alloc: &mut Alloc, devmem: &mut crate::devmem::DevMem, win: &mut crate::window::Window) -> u64 {
    let Some(frames) = devmem.frames(&mut alloc.slots, 0x0901_0000, 4096) else { return 0 };
    win.with_frame_attrs(&crate::component::abs_of(frames[0]), sel4::VmAttributes::EXECUTE_NEVER,
        |p| unsafe { (p as *const u32).read_volatile() as u64 * 1_000_000_000 })
}

/// The RTC registers read_ns uses, status register B last, through I/O
/// port capability `port`; None if a port access fails.
#[cfg(target_arch = "x86_64")]
fn sample(port: u64) -> Option<[u8; 8]> {
    let read = |reg: u8| -> Option<u8> {
        sel4::with_ipc_buffer_mut(|ipc| {
            if ipc.inner_mut().seL4_X86_IOPort_Out8(port, CMOS as u64, reg as u64) != 0 {
                return None;
            }
            let r = ipc.inner_mut().seL4_X86_IOPort_In8(port, CMOS + 1);
            if r.error != 0 { None } else { Some(r.result as u8) }
        })
    };
    // Read outside of updates, until two consecutive reads agree.
    let once = || -> Option<[u8; 8]> {
        while read(0x0a)? & 0x80 != 0 {}
        let mut v = [0u8; 8];
        for (x, reg) in v.iter_mut().zip([0x00, 0x02, 0x04, 0x07, 0x08, 0x09, 0x32, 0x0b]) {
            *x = read(reg)?;
        }
        Some(v)
    };
    let mut t = once()?;
    loop {
        let u = once()?;
        if u == t {
            return Some(t);
        }
        t = u;
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
#[cfg(target_arch = "x86_64")]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
