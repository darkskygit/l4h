//! Guest console output: the records in each vCPU's output ring
//! (abi::ConsoleOut, in its event page), printed in seq order into the ring
//! l4h-vmmgr reads and optionally to the serial port, each line prefixed
//! with the guest id (except dom0).  The guest writes the rings and never
//! waits for us; what it gave up on, it counts in `lost`, which we report.
//! A missing sequence gets 100 ms to arrive before we report the gap and
//! skip it. Records arriving after they were skipped are discarded.

use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use l4h_abi as abi;
use l4h_rt::{cap, host, sel4};

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
    next_seq: u32,
    gap_ticks: u64,
    gap_deadline: Option<u64>,
    /// Deterministic publication between sampling two rings' heads.
    #[cfg(test)]
    pub after_head: Option<fn(usize)>,
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
    pub fn new(serial: bool, prefix: Option<u32>, event: u32, hz: u64) -> Self {
        Console {
            ring: host::VMM_CONSOLE_RING_VA as *mut host::ConsoleRing,
            serial,
            prefix,
            line_start: true,
            event,
            tail: [0; abi::MAX_VCPUS],
            lost: [0; abi::MAX_VCPUS],
            next_seq: 0,
            // 100 ms leaves room for the guest's capped scheduling periods.
            gap_ticks: hz.div_ceil(10).max(1),
            gap_deadline: None,
            #[cfg(test)]
            after_head: None,
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
        #[cfg(test)]
        if let Some(after_head) = self.after_head.take() {
            after_head(v);
        }
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

    fn timer(&self, deadline: u64) {
        let tcb = cap::<sel4::cap_type::Tcb>(host::SLOT_TCB);
        let error = sel4::with_ipc_buffer_mut(|b| b.inner_mut().seL4_TCB_SetTimer(tcb.bits(), deadline, host::VMM_BADGE_CONSOLE));
        assert_eq!(error, 0, "console gap timer");
    }

    /// SetTimer drops an expiry while the TCB's notification is unbound.
    /// Call after rebinding, before sleeping, even for an unchanged deadline.
    /// Like other TCB invocations, this clobbers the IPC buffer.
    pub fn rearm_timer(&self) {
        if let Some(deadline) = self.gap_deadline {
            self.timer(deadline);
        }
    }

    /// Keep the first deadline for this missing sequence: kicks must not
    /// extend it. Clear it only when we advance next_seq or have no backlog.
    fn waiting_for_gap(&mut self) -> bool {
        let now = host::ticks::now();
        let deadline = *self.gap_deadline.get_or_insert_with(|| now.saturating_add(self.gap_ticks));
        now < deadline
    }

    /// Print at most one ring's worth in sequence order. True means work
    /// can continue immediately, after polling for other work. A gap arms
    /// a one-shot timer and returns false, so the VMM can sleep meanwhile.
    pub fn drain(&mut self, vcpus: &Vcpus) -> bool {
        self.drain_limit(vcpus, SIZE, false)
    }

    /// Before poweroff, allow enough bytes for all currently queued data.
    /// Skip gaps immediately: a stopped or faulted writer may never finish.
    /// Still bounded if another vCPU continues writing while we print.
    pub fn flush(&mut self, vcpus: &Vcpus) {
        self.drain_limit(vcpus, SIZE * vcpus.n, true);
        if self.gap_deadline.take().is_some() {
            self.timer(0);
        }
    }

    fn drain_limit(&mut self, vcpus: &Vcpus, mut budget: usize, flush: bool) -> bool {
        let old_deadline = self.gap_deadline;
        let mut rechecked_gap = false;
        loop {
            let mut next: Option<(usize, u32, u32)> = None;
            for v in 0..vcpus.n {
                let Some(avail) = self.pending(v) else { continue };
                let seq = word(out(v), self.tail[v]);
                if next.is_none_or(|(_, s, _)| (seq.wrapping_sub(self.next_seq) as i32) < (s.wrapping_sub(self.next_seq) as i32)) {
                    next = Some((v, seq, avail));
                }
            }
            let Some((v, seq, avail)) = next else { break };
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
            let order = seq.wrapping_sub(self.next_seq) as i32;
            if order > 0 {
                // A ring sampled empty may have published the missing
                // record before a later ring was sampled. Recheck once;
                // never spin waiting for an unpublished reservation.
                if !rechecked_gap {
                    rechecked_gap = true;
                    continue;
                }
                if !flush && self.waiting_for_gap() {
                    break;
                }
                if !self.line_start {
                    self.put(b'\n');
                }
                let _ = writeln!(self, "[l4h: skipped {} missing console records]", order);
            }
            if order >= 0 {
                for i in 0..len {
                    self.put(byte(p, t.wrapping_add(REC as u32 + i)));
                }
                self.next_seq = seq.wrapping_add(1);
                self.gap_deadline = None;
            }
            // A late record was already counted when we skipped its gap.
            // Consume it without printing, including against the budget.
            self.consume(v, (t + bytes as u32) % WRAP);
            budget -= bytes;
            if budget == 0 {
                break;
            }
        }

        let mut room = false;
        let mut more = false;
        let mut future = false;
        for v in 0..vcpus.n {
            let p = out(v);
            // This check follows publishing tail: a writer that saw an old
            // tail need not kick, so carry its new data into the next batch.
            let avail = self.pending(v).unwrap_or(0) as usize;
            if avail != 0 {
                let seq = word(p, self.tail[v]);
                let len = word(p, self.tail[v].wrapping_add(4)) as usize;
                // Invalid and late records also need another bounded pass.
                more |= len > avail - REC || (seq.wrapping_sub(self.next_seq) as i32) <= 0;
                future |= (seq.wrapping_sub(self.next_seq) as i32) > 0;
            }
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
        if !more {
            if future {
                more = flush || !self.waiting_for_gap();
            } else {
                self.gap_deadline = None;
            }
        }
        if self.gap_deadline != old_deadline {
            self.timer(self.gap_deadline.unwrap_or(0));
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
