//! Guest console output: the records in each vCPU's output ring
//! (abi::ConsoleOut, in its event page), printed in seq order into the ring
//! l4h-vmmgr reads and optionally to the serial port, each line prefixed
//! with the guest id (except dom0).  The guest writes the rings and never
//! waits for us; what it gave up on, it counts in `lost`, which we report.

use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_abi as abi;
use l4h_rt::host;
use l4h_rt::sel4;

use crate::guest::Vcpus;

const REC: usize = core::mem::size_of::<abi::ConsoleRec>();
const SIZE: usize = abi::CONSOLE_OUT_SIZE;
const WRAP: u32 = abi::CONSOLE_OUT_WRAP;

pub struct Console {
    ring: *mut host::ConsoleRing,
    serial: bool,
    prefix: Option<u32>,
    line_start: bool,
    /// The console device's event, raised when a ring wanted room.
    event: u32,
    /// Per vCPU: how far we read its ring, and the `lost` we reported.
    tail: [u32; abi::MAX_VCPUS],
    lost: [u32; abi::MAX_VCPUS],
}

fn out(v: usize) -> *mut abi::ConsoleOut {
    unsafe { addr_of_mut!((*(host::vmm_event_page_va(v) as *mut abi::EventPage)).console) }
}

fn byte(p: *mut abi::ConsoleOut, off: u32) -> u8 {
    unsafe { addr_of!((*p).data[off as usize % SIZE]).read_volatile() }
}

fn word(p: *mut abi::ConsoleOut, off: u32) -> u32 {
    u32::from_le_bytes(core::array::from_fn(|i| byte(p, off.wrapping_add(i as u32))))
}

impl Console {
    pub fn new(serial: bool, prefix: Option<u32>, event: u32) -> Self {
        Console {
            ring: host::VMM_CONSOLE_RING_VA as *mut host::ConsoleRing,
            serial,
            prefix,
            line_start: true,
            event,
            tail: [0; abi::MAX_VCPUS],
            lost: [0; abi::MAX_VCPUS],
        }
    }

    fn put(&mut self, c: u8) {
        unsafe {
            let head = addr_of!((*self.ring).head).read_volatile();
            addr_of_mut!((*self.ring).data[head as usize % host::CONSOLE_RING_DATA]).write_volatile(c);
            fence(Ordering::Release);
            addr_of_mut!((*self.ring).head).write_volatile(head + 1);
        }
        if self.serial {
            if self.line_start {
                if let Some(id) = self.prefix {
                    sel4::debug_print!("[vm{}] ", id);
                }
            }
            sel4::debug_put_char(c);
        }
        self.line_start = c == b'\n';
    }

    /// Bytes from vCPU v's tail to its head, or None if there are none;
    /// a ring the guest left inconsistent is skipped.
    fn pending(&mut self, v: usize) -> Option<u32> {
        let p = out(v);
        let head = unsafe { addr_of!((*p).head).read_volatile() };
        if head >= WRAP {
            self.consume(v, head % WRAP);
            return None;
        }
        let avail = (head + WRAP - self.tail[v]) % WRAP;
        if avail == 0 {
            return None;
        }
        fence(Ordering::Acquire);
        // The guest publishes whole records only.
        if (avail as usize) < REC || avail as usize > SIZE {
            self.consume(v, head);
            return None;
        }
        Some(avail)
    }

    fn consume(&mut self, v: usize, to: u32) {
        self.tail[v] = to;
        unsafe { addr_of_mut!((*out(v)).tail).write_volatile(to) };
        // The guest kicks only if it sees we caught up, after writing its
        // head: the next look at the heads comes after this.
        fence(Ordering::SeqCst);
    }

    /// Print at most one ring's worth, oldest first. Return whether more
    /// remains, so the event loop polls for other work before draining again.
    pub fn drain(&mut self, vcpus: &Vcpus) -> bool {
        self.drain_limit(vcpus, SIZE)
    }

    /// Before poweroff, allow enough bytes for all currently queued data.
    /// Still bounded if another vCPU continues writing while we print.
    pub fn flush(&mut self, vcpus: &Vcpus) {
        self.drain_limit(vcpus, SIZE * vcpus.n);
    }

    fn drain_limit(&mut self, vcpus: &Vcpus, mut budget: usize) -> bool {
        loop {
            let mut next: Option<(usize, u32, u32)> = None;
            for v in 0..vcpus.n {
                let Some(avail) = self.pending(v) else { continue };
                let seq = word(out(v), self.tail[v]);
                if next.is_none_or(|(_, s, _)| (seq.wrapping_sub(s) as i32) < 0) {
                    next = Some((v, seq, avail));
                }
            }
            let Some((v, _, avail)) = next else { break };
            let p = out(v);
            let t = self.tail[v];
            let len = word(p, t.wrapping_add(4));
            if len as usize > avail as usize - REC {
                self.consume(v, (t + avail) % WRAP);
                budget = budget.saturating_sub(avail as usize);
                if budget == 0 {
                    break;
                }
                continue;
            }
            let bytes = REC + len as usize;
            if bytes > budget {
                break;
            }
            for i in 0..len {
                self.put(byte(p, t.wrapping_add(REC as u32 + i)));
            }
            self.consume(v, (t + bytes as u32) % WRAP);
            budget -= bytes;
            if budget == 0 {
                break;
            }
        }

        let mut room = false;
        let mut more = false;
        for v in 0..vcpus.n {
            let p = out(v);
            // This check follows publishing tail: a writer that saw an old
            // tail need not kick, so carry its new data into the next batch.
            let avail = self.pending(v).unwrap_or(0) as usize;
            more |= avail != 0;
            let lost = unsafe { addr_of!((*p).lost).read_volatile() };
            if lost != self.lost[v] {
                let n = lost.wrapping_sub(self.lost[v]);
                self.lost[v] = lost;
                if !self.line_start {
                    self.put(b'\n');
                }
                let _ = writeln!(self, "[l4h: vCPU {} lost {} bytes of console output]", v, n);
            }
            if SIZE - avail > REC && unsafe { addr_of!((*p).want_space).read_volatile() } != 0 {
                unsafe { addr_of_mut!((*p).want_space).write_volatile(0) };
                room = true;
            }
        }
        if room {
            vcpus.raise(0, self.event);
        }
        more
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for c in s.bytes() {
            self.put(c);
        }
        Ok(())
    }
}
