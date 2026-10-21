//! TPM 2.0 transports at locality 0: the FIFO (TIS) and CRB interfaces of
//! the TCG PC Client Platform TPM Profile.  Commands are polled: trustd
//! takes no interrupt.

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use l4h_rt::host;
use l4h_rt::host::trust::{TPM_CRB, TPM_TIS, TrustdConfig};

/// The longest a command may take: key generation on a slow TPM.
const TIMEOUT_SECS: u64 = 120;
/// Largest response we accept.
const MAX_RESPONSE: usize = 4096;
/// Warnings that ask for the command again.
const RC_YIELDED: u32 = 0x908;
const RC_TESTING: u32 = 0x90a;
const RC_RETRY: u32 = 0x922;

pub enum Error {
    /// The TPM did not answer in time (what it was waiting for).
    Timeout(&'static str),
    /// A malformed response or an unusable interface.
    Bad(&'static str),
    /// A TPM response code.
    Rc(u32),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Error::Timeout(what) => write!(f, "timeout waiting for {}", what),
            Error::Bad(why) => f.write_str(why),
            Error::Rc(rc) => write!(f, "TPM error {:#x}", rc),
        }
    }
}

pub struct Tpm {
    iface: u32,
    regs: usize,
    cmd: (usize, usize),
    rsp: (usize, usize),
    hz: u64,
}

// TIS (FIFO) registers.
const TIS_ACCESS: usize = 0x00;
const TIS_STS: usize = 0x18;
const TIS_FIFO: usize = 0x24;
const ACCESS_VALID: u8 = 0x80;
const ACCESS_ACTIVE: u8 = 0x20;
const ACCESS_REQUEST: u8 = 0x02;
const STS_VALID: u32 = 0x80;
const STS_READY: u32 = 0x40;
const STS_GO: u32 = 0x20;
const STS_DATA: u32 = 0x10;
const STS_EXPECT: u32 = 0x08;

// CRB registers.
const CRB_LOC_STATE: usize = 0x00;
const CRB_LOC_CTRL: usize = 0x08;
const CRB_CTRL_REQ: usize = 0x40;
const CRB_CTRL_STS: usize = 0x44;
const CRB_CTRL_START: usize = 0x4c;
const LOC_ASSIGNED: u32 = 0x02;
const LOC_VALID: u32 = 0x80;
const REQ_READY: u32 = 0x01;
const REQ_IDLE: u32 = 0x02;
const STS_FATAL: u32 = 0x01;

impl Tpm {
    pub fn new(cfg: &TrustdConfig, hz: u64) -> Result<Self, Error> {
        let base = host::trust::TPM_VA;
        let size = cfg.mmio_size as usize;
        let inside = |off: u64, len: u64| off.checked_add(len).is_some_and(|e| e <= size as u64);
        if cfg.iface == TPM_CRB && !(inside(cfg.cmd_off, cfg.cmd_size) && inside(cfg.rsp_off, cfg.rsp_size)) {
            return Err(Error::Bad("CRB buffers outside the mapped registers"));
        }
        if cfg.iface != TPM_TIS && cfg.iface != TPM_CRB {
            return Err(Error::Bad("unknown TPM interface"));
        }
        Ok(Tpm {
            iface: cfg.iface,
            regs: base,
            cmd: (base + cfg.cmd_off as usize, cfg.cmd_size as usize),
            rsp: (base + cfg.rsp_off as usize, cfg.rsp_size as usize),
            hz,
        })
    }

    fn r8(&self, off: usize) -> u8 {
        unsafe { read_volatile((self.regs + off) as *const u8) }
    }

    fn w8(&self, off: usize, v: u8) {
        unsafe { write_volatile((self.regs + off) as *mut u8, v) }
    }

    fn r32(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.regs + off) as *const u32) }
    }

    fn w32(&self, off: usize, v: u32) {
        unsafe { write_volatile((self.regs + off) as *mut u32, v) }
    }

    /// Spin until `done` holds.
    fn wait(&self, what: &'static str, mut done: impl FnMut(&Self) -> bool) -> Result<(), Error> {
        let deadline = host::ticks::now() + self.hz * TIMEOUT_SECS;
        while !done(self) {
            if host::ticks::now() > deadline {
                return Err(Error::Timeout(what));
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// Send `cmd` and return the response, whose code is checked.  The
    /// TPM may ask for a command again (busy, self-testing): we do, for a
    /// while.
    pub fn exec(&mut self, cmd: &[u8]) -> Result<Vec<u8>, Error> {
        let deadline = host::ticks::now() + self.hz * TIMEOUT_SECS;
        loop {
            let rsp = match self.iface {
                TPM_TIS => self.exec_tis(cmd),
                _ => self.exec_crb(cmd),
            }?;
            if rsp.len() < 10 {
                return Err(Error::Bad("short response"));
            }
            match u32::from_be_bytes(rsp[6..10].try_into().unwrap()) {
                0 => return Ok(rsp),
                RC_YIELDED | RC_TESTING | RC_RETRY if host::ticks::now() < deadline => {
                    let pause = host::ticks::now() + self.hz / 1000;
                    while host::ticks::now() < pause {
                        core::hint::spin_loop();
                    }
                }
                rc => return Err(Error::Rc(rc)),
            }
        }
    }

    fn exec_tis(&mut self, cmd: &[u8]) -> Result<Vec<u8>, Error> {
        self.w8(TIS_ACCESS, ACCESS_REQUEST);
        self.wait("locality", |t| t.r8(TIS_ACCESS) & (ACCESS_VALID | ACCESS_ACTIVE) == ACCESS_VALID | ACCESS_ACTIVE)?;
        let r = self.tis_command(cmd);
        // Back to idle, whatever happened, and give up the locality.
        self.w8(TIS_STS, STS_READY as u8);
        self.w8(TIS_ACCESS, ACCESS_ACTIVE);
        r
    }

    fn burst(&self) -> usize {
        ((self.r32(TIS_STS) >> 8) & 0xffff) as usize
    }

    fn tis_command(&mut self, cmd: &[u8]) -> Result<Vec<u8>, Error> {
        self.w8(TIS_STS, STS_READY as u8);
        self.wait("command ready", |t| t.r32(TIS_STS) & STS_READY != 0)?;
        let mut sent = 0;
        while sent < cmd.len() {
            self.wait("burst count", |t| t.burst() != 0)?;
            let n = self.burst().min(cmd.len() - sent);
            for &b in &cmd[sent..sent + n] {
                self.w8(TIS_FIFO, b);
            }
            sent += n;
        }
        self.wait("status", |t| t.r32(TIS_STS) & STS_VALID != 0)?;
        if self.r32(TIS_STS) & STS_EXPECT != 0 {
            return Err(Error::Bad("TPM expects more command bytes"));
        }
        self.w8(TIS_STS, STS_GO as u8);
        self.wait("response", |t| t.r32(TIS_STS) & (STS_VALID | STS_DATA) == STS_VALID | STS_DATA)?;
        let mut rsp = Vec::new();
        let mut want = 10;
        while rsp.len() < want {
            self.wait("response data", |t| t.burst() != 0)?;
            let n = self.burst().min(want - rsp.len());
            for _ in 0..n {
                rsp.push(self.r8(TIS_FIFO));
            }
            if want == 10 && rsp.len() >= 10 {
                want = u32::from_be_bytes(rsp[2..6].try_into().unwrap()) as usize;
                if !(10..=MAX_RESPONSE).contains(&want) {
                    return Err(Error::Bad("response size"));
                }
            }
        }
        Ok(rsp)
    }

    fn exec_crb(&mut self, cmd: &[u8]) -> Result<Vec<u8>, Error> {
        if cmd.len() > self.cmd.1 {
            return Err(Error::Bad("command larger than the CRB buffer"));
        }
        self.w32(CRB_LOC_CTRL, 1);
        self.wait("locality", |t| t.r32(CRB_LOC_STATE) & (LOC_VALID | LOC_ASSIGNED) == LOC_VALID | LOC_ASSIGNED)?;
        let r = self.crb_command(cmd);
        self.w32(CRB_CTRL_REQ, REQ_IDLE);
        self.w32(CRB_LOC_CTRL, 2);
        r
    }

    fn crb_command(&mut self, cmd: &[u8]) -> Result<Vec<u8>, Error> {
        self.w32(CRB_CTRL_REQ, REQ_READY);
        self.wait("command ready", |t| t.r32(CRB_CTRL_REQ) & REQ_READY == 0)?;
        if self.r32(CRB_CTRL_STS) & STS_FATAL != 0 {
            return Err(Error::Bad("TPM in failure mode"));
        }
        for (i, &b) in cmd.iter().enumerate() {
            unsafe { write_volatile((self.cmd.0 + i) as *mut u8, b) };
        }
        self.w32(CRB_CTRL_START, 1);
        self.wait("response", |t| t.r32(CRB_CTRL_START) & 1 == 0)?;
        let byte = |i: usize| unsafe { read_volatile((self.rsp.0 + i) as *const u8) };
        let mut rsp: Vec<u8> = (0..10).map(byte).collect();
        let n = u32::from_be_bytes(rsp[2..6].try_into().unwrap()) as usize;
        if !(10..=MAX_RESPONSE.min(self.rsp.1)).contains(&n) {
            return Err(Error::Bad("response size"));
        }
        rsp.extend((10..n).map(byte));
        Ok(rsp)
    }
}
