//! MC146818 calendar access, independent of the port I/O transport.

const CONTROL: u8 = 0x0b;
const SET: u8 = 0x80;
const BINARY: u8 = 0x04;
const H24: u8 = 0x02;
const REGS: [u8; 7] = [0x00, 0x02, 0x04, 0x06, 0x07, 0x08, 0x09];

pub trait Registers {
    fn read(&mut self, reg: u8) -> Result<u8, ()>;
    fn write(&mut self, reg: u8, value: u8) -> Result<(), ()>;
}

#[derive(Debug, PartialEq)]
pub enum Error { Invalid, Io }

fn encode(value: u8, control: u8) -> u8 {
    if control & BINARY != 0 { value } else { (value / 10) << 4 | value % 10 }
}

fn decode(value: u8, control: u8) -> u8 {
    if control & BINARY != 0 { value } else { (value & 0x0f) + (value >> 4) * 10 }
}

/// Calendar years 1970..=2099. MC146818's leap-year rule repeats every
/// four years, so 2100 is not representable. Without a century register,
/// use 2000..=2099, matching the boot reader's default century.
pub fn set_secs(rtc: &mut impl Registers, century: Option<u8>, seconds: u64) -> Result<(), Error> {
    if seconds >= 4_102_444_800 || (century.is_none() && seconds < 946_684_800) {
        return Err(Error::Invalid);
    }
    let days = seconds / 86400;
    let mut left = days;
    let mut year = 1970;
    loop {
        let n = if year % 4 == 0 { 366 } else { 365 };
        if left < n { break; }
        left -= n;
        year += 1;
    }
    let months = [31, if year % 4 == 0 { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    while left >= months[month] { left -= months[month]; month += 1; }
    let hour = (seconds / 3600 % 24) as u8;
    let control = rtc.read(CONTROL).map_err(|_| Error::Io)?;
    if control == 0xff { return Err(Error::Io); }
    let raw_hour = if control & H24 != 0 { encode(hour, control) } else {
        encode(if hour % 12 == 0 { 12 } else { hour % 12 }, control) | if hour >= 12 { 0x80 } else { 0 }
    };
    let values = [
        encode((seconds % 60) as u8, control),
        encode((seconds / 60 % 60) as u8, control),
        raw_hour,
        encode(((days + 4) % 7 + 1) as u8, control), // Sunday = 1
        encode((left + 1) as u8, control),
        encode((month + 1) as u8, control),
        encode((year % 100) as u8, control),
    ];
    // SET inhibits calendar updates, including any update in progress.
    // Preserve the binary/BCD and 12/24-hour modes and interrupt enables.
    // Always attempt to restore control, even after a failed data write.
    rtc.write(CONTROL, control | SET).map_err(|_| Error::Io)?;
    let result = (|| {
        for (reg, value) in REGS.into_iter().zip(values) { rtc.write(reg, value)?; }
        if let Some(reg) = century { rtc.write(reg, encode((year / 100) as u8, control))?; }
        Ok(())
    })();
    let restore = rtc.write(CONTROL, control);
    result.and(restore).map_err(|_| Error::Io)
}

pub fn read_ns(rtc: &mut impl Registers, century: Option<u8>) -> Option<u64> {
    let mut last = None;
    // Bound reads so absent or faulty hardware cannot hang host boot.
    for _ in 0..10_000 {
        if rtc.read(0x0a).ok()? & 0x80 != 0 { continue; }
        let mut v = [0u8; 9];
        for (value, reg) in v.iter_mut().zip(REGS) { *value = rtc.read(reg).ok()?; }
        v[7] = match century { Some(reg) => rtc.read(reg).ok()?, None => 0 };
        v[8] = rtc.read(CONTROL).ok()?;
        if last != Some(v) { last = Some(v); continue; }
        let b = v[8];
        let century = match if century.is_some() { decode(v[7], b) } else { 20 } {
            c @ 19..=21 => c as i64,
            _ => 20,
        };
        let year = century * 100 + decode(v[6], b) as i64;
        let mon = decode(v[5], b) as i64;
        let day = decode(v[4], b) as i64;
        let mut hour = decode(v[2] & 0x7f, b) as u64;
        if b & H24 == 0 { hour = hour % 12 + if v[2] & 0x80 != 0 { 12 } else { 0 }; }
        let min = decode(v[1], b) as u64;
        let sec = decode(v[0], b) as u64;
        if !(1970..=2199).contains(&year) || !(1..=12).contains(&mon) || !(1..=31).contains(&day)
            || hour >= 24 || min >= 60 || sec >= 60 { return None; }
        let days = days_from_civil(year, mon, day);
        if days < 0 { return None; }
        return Some((days as u64 * 86400 + hour * 3600 + min * 60 + sec) * 1_000_000_000);
    }
    None
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
