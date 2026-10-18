//! Wall-clock time at boot, from the CMOS real-time clock (x86).  Guests
//! derive their wall clock from it (L4H_HC_GET_TIME) and may correct it
//! with NTP.

use sel4::cap_type;

use crate::mem::Alloc;

const CMOS: u16 = 0x70;

/// Nanoseconds since the Unix epoch, read now; 0 if there is no RTC.
#[cfg(target_arch = "x86_64")]
pub fn read_ns(alloc: &mut Alloc) -> u64 {
    use sel4::init_thread::slot;
    let s = alloc.slots.alloc();
    let dst = slot::CNODE.cap().absolute_cptr(sel4::init_thread::Slot::<cap_type::Null>::from_index(s).cap());
    if slot::IO_PORT_CONTROL.cap().ioport_control_issue(CMOS as u64, CMOS as u64 + 1, &dst).is_err() {
        return 0;
    }
    let port = s as u64;
    let read = |reg: u8| -> u8 {
        sel4::with_ipc_buffer_mut(|ipc| {
            ipc.inner_mut().seL4_X86_IOPort_Out8(port, CMOS as u64, reg as u64);
            ipc.inner_mut().seL4_X86_IOPort_In8(port, CMOS + 1).result as u8
        })
    };
    // Read until two consecutive reads agree, outside of updates.
    let sample = || {
        while read(0x0a) & 0x80 != 0 {}
        [read(0x00), read(0x02), read(0x04), read(0x07), read(0x08), read(0x09), read(0x32)]
    };
    let mut t = sample();
    loop {
        let u = sample();
        if u == t {
            break;
        }
        t = u;
    }
    let _ = dst.delete();
    alloc.slots.free(s);
    let b = read(0x0b);
    let bcd = |v: u8| if b & 0x04 == 0 { (v & 0x0f) + (v >> 4) * 10 } else { v };
    let [sec, min, hour_raw, day, mon, year, century] = t;
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

#[cfg(not(target_arch = "x86_64"))]
pub fn read_ns(_alloc: &mut Alloc) -> u64 {
    let _ = (CMOS, days_from_civil);
    0
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
