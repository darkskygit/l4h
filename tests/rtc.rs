// Run with tests/run-rtctest.sh; exercises the host's CMOS implementation.
#[path = "../host/l4h-root/src/rtc/cmos.rs"]
mod cmos;
use cmos::{Error, Registers};

struct Rtc {
    regs: [u8; 128],
    writes: Vec<(u8, u8)>,
    fail: Option<u8>,
}

impl Rtc {
    fn new(control: u8) -> Self {
        let mut regs = [0x55; 128];
        regs[0x0a] = 0x26;
        regs[0x0b] = control;
        Self { regs, writes: Vec::new(), fail: None }
    }
}

impl Registers for Rtc {
    fn read(&mut self, reg: u8) -> Result<u8, ()> { Ok(self.regs[reg as usize]) }
    fn write(&mut self, reg: u8, value: u8) -> Result<(), ()> {
        if self.fail == Some(reg) { return Err(()); }
        self.writes.push((reg, value));
        self.regs[reg as usize] = value;
        Ok(())
    }
}

#[test]
fn leap_day_and_weekday_in_bcd() {
    let mut rtc = Rtc::new(0x02);
    // 2000-02-29 12:34:56 UTC, Tuesday.
    cmos::set_secs(&mut rtc, Some(0x32), 951_827_696).unwrap();
    for (reg, expected) in [(0, 0x56), (2, 0x34), (4, 0x12), (6, 3), (7, 0x29), (8, 2), (9, 0), (0x32, 0x20)] {
        assert_eq!(rtc.regs[reg], expected, "register {reg}");
    }
}

#[test]
fn midnight_noon_and_pm_in_all_four_modes() {
    for (control, hours) in [(0x00, [0x12, 0x92, 0x91]), (0x02, [0, 0x12, 0x23]),
                             (0x04, [12, 0x8c, 0x8b]), (0x06, [0, 12, 23])] {
        for (hour, raw) in [0, 12, 23].into_iter().zip(hours) {
            let mut rtc = Rtc::new(control | 0x70); // interrupt enables
            let seconds = 946_684_800 + hour * 3600;
            cmos::set_secs(&mut rtc, Some(0x32), seconds).unwrap();
            assert_eq!(rtc.regs[4], raw);
            assert_eq!(rtc.regs[0x0b], control | 0x70);
            assert_eq!(rtc.writes.first(), Some(&(0x0b, control | 0xf0)));
            assert_eq!(rtc.writes.last(), Some(&(0x0b, control | 0x70)));
            assert_eq!(cmos::read_ns(&mut rtc, Some(0x32)), Some(seconds * 1_000_000_000));
        }
    }
}

#[test]
fn every_day_roundtrips_across_leap_years_and_centuries() {
    let mut rtc = Rtc::new(0x02);
    for day in 0..47_482 { // 1970-01-01 through 2099-12-31
        let seconds = day * 86400 + 23 * 3600 + 59 * 60 + 59;
        cmos::set_secs(&mut rtc, Some(0x32), seconds).unwrap();
        assert_eq!(cmos::read_ns(&mut rtc, Some(0x32)), Some(seconds * 1_000_000_000));
        rtc.writes.clear();
    }
}

#[test]
fn no_century_register_leaves_nvram_untouched() {
    let mut rtc = Rtc::new(0x06);
    cmos::set_secs(&mut rtc, None, 946_684_800).unwrap();
    assert_eq!(rtc.regs[0x32], 0x55);
    assert_eq!(cmos::read_ns(&mut rtc, None), Some(946_684_800_000_000_000));
    assert!(rtc.writes.iter().all(|&(reg, _)| reg < 0x0c));
}

#[test]
fn invalid_dates_do_not_touch_hardware() {
    for (century, seconds) in [(None, 0), (None, 946_684_799), (Some(0x32), 4_102_444_800), (Some(0x32), u64::MAX)] {
        let mut rtc = Rtc::new(0x02);
        assert_eq!(cmos::set_secs(&mut rtc, century, seconds), Err(Error::Invalid));
        assert!(rtc.writes.is_empty());
    }
}

#[test]
fn data_write_failure_restores_control_and_reports_io_error() {
    for reg in [0, 2, 4, 6, 7, 8, 9, 0x32] {
        let mut rtc = Rtc::new(0x72);
        rtc.fail = Some(reg);
        assert_eq!(cmos::set_secs(&mut rtc, Some(0x32), 951_827_696), Err(Error::Io));
        assert_eq!(rtc.regs[0x0b], 0x72);
        assert_eq!(rtc.writes.last(), Some(&(0x0b, 0x72)));
    }
}

#[test]
fn absent_or_stuck_hardware_does_not_hang() {
    let mut rtc = Rtc::new(0xff);
    assert_eq!(cmos::set_secs(&mut rtc, Some(0x32), 951_827_696), Err(Error::Io));
    assert!(rtc.writes.is_empty());
    rtc.regs[0x0a] = 0x80;
    assert_eq!(cmos::read_ns(&mut rtc, Some(0x32)), None);
}
