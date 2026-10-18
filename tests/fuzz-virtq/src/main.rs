//! Feed l4h-vmm's virtqueue code (host/l4h-vmm/src/virtq.rs, compiled
//! here unchanged) with random guest memory: queue setup, avail rings and
//! descriptor tables are all garbage.  It must never touch memory outside
//! guest RAM, panic or loop forever.

#![allow(dead_code)]

extern crate alloc;

mod guest {
    use std::cell::RefCell;

    /// Guest RAM as a byte array; any access outside it panics, which the
    /// harness reports as a failure.
    pub struct GuestMem {
        pub size: u64,
        pub ram: RefCell<Vec<u8>>,
    }

    impl GuestMem {
        pub fn valid(&self, gpa: u64, len: u64) -> bool {
            gpa.checked_add(len).is_some_and(|end| end <= self.size)
        }

        pub fn read<T: Copy>(&self, gpa: u64) -> T {
            let n = core::mem::size_of::<T>();
            assert!(self.valid(gpa, n as u64), "read outside guest RAM: {gpa:#x}");
            let ram = self.ram.borrow();
            unsafe { (ram.as_ptr().add(gpa as usize) as *const T).read_unaligned() }
        }

        pub fn write<T: Copy>(&self, gpa: u64, v: T) {
            let n = core::mem::size_of::<T>();
            assert!(self.valid(gpa, n as u64), "write outside guest RAM: {gpa:#x}");
            let mut ram = self.ram.borrow_mut();
            unsafe { (ram.as_mut_ptr().add(gpa as usize) as *mut T).write_unaligned(v) }
        }
    }
}

#[path = "../../../host/l4h-vmm/src/virtq.rs"]
mod virtq;

use guest::GuestMem;
use std::cell::RefCell;

/// xorshift64*
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn main() {
    let iterations: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let size = 64 * 1024u64;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut chains = 0u64;
    let mut bufs = Vec::new();
    for _ in 0..iterations {
        let mut ram = vec![0u8; size as usize];
        for b in ram.iter_mut() {
            *b = rng.next() as u8;
        }
        let mem = GuestMem { size, ram: RefCell::new(ram) };
        // Mostly plausible queue layouts, sometimes wild ones.
        let num = 1u32 << rng.below(9);
        let pick = |rng: &mut Rng| if rng.below(4) == 0 { rng.next() } else { rng.below(size) };
        let (desc, avail, used) = (pick(&mut rng), pick(&mut rng), pick(&mut rng));
        let mut q = virtq::Queue::default();
        if !q.setup(&mem, num, 256, desc, avail, used) {
            continue;
        }
        // Make chains mostly local so that they are followed for a while.
        for i in 0..num as u64 {
            if rng.below(2) == 0 {
                let d = desc + 16 * i;
                mem.write::<u16>(d + 12, (rng.below(8)) as u16);
                mem.write::<u16>(d + 14, rng.below(num as u64 + 2) as u16);
                mem.write::<u32>(d + 8, rng.below(4096) as u32);
                mem.write::<u64>(d, rng.below(size));
            }
        }
        for _ in 0..rng.below(2 * num as u64) {
            let Some(head) = q.pop(&mem) else { break };
            let _ = q.chain(&mem, head, &mut bufs);
            for b in &bufs {
                assert!(mem.valid(b.gpa, b.len as u64), "chain buffer outside guest RAM");
            }
            chains += 1;
            q.push(&mem, head, rng.below(1 << 16) as u32);
            let _ = q.wants_interrupt(&mem);
        }
    }
    println!("fuzz-virtq: {iterations} queues, {chains} chains parsed, no violations");
}
