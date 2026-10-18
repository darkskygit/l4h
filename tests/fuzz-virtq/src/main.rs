//! Feed l4h-vmm's virtqueue code (host/l4h-vmm/src/virtq.rs and mem.rs,
//! compiled here unchanged) with random guest memory: queue setup, avail
//! rings and descriptor tables are all garbage.  It must never touch memory
//! outside guest RAM, panic or loop forever.  Also feeds netd's offload
//! code (host/l4h-netd/src/offload.rs) random virtio-net headers and checks
//! its TCP segmentation.

#![allow(dead_code)]

extern crate alloc;

#[path = "../../../host/l4h-vmm/src/mem.rs"]
mod mem;

/// virtq.rs uses crate::guest::GuestMem: the production one, over a
/// buffer here.
mod guest {
    pub use crate::mem::GuestMem;
}

#[path = "../../../host/l4h-vmm/src/virtq.rs"]
mod virtq;

#[path = "../../../host/l4h-netd/src/offload.rs"]
mod offload;

use guest::GuestMem;

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
        let mut ram: Vec<u64> = (0..size / 8).map(|_| rng.next()).collect();
        let mem = GuestMem { base: ram.as_mut_ptr() as usize, size };
        // Mostly plausible queue layouts, sometimes wild ones.
        let num = 1u32 << rng.below(9);
        let pick = |rng: &mut Rng| if rng.below(4) == 0 { rng.next() } else { rng.below(size) };
        // Mostly aligned as required, sometimes not (setup must refuse).
        let align = |rng: &mut Rng, v: u64, a: u64| if rng.below(8) == 0 { v } else { v & !(a - 1) };
        let (desc, avail, used) = (pick(&mut rng), pick(&mut rng), pick(&mut rng));
        let (desc, avail, used) = (align(&mut rng, desc, 16), align(&mut rng, avail, 2), align(&mut rng, used, 4));
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
    let segs = offload_check(&mut rng, iterations / 4);
    println!("fuzz-virtq: offload: {} frames, {segs} segments checked", iterations / 4);
}

fn csum(data: &[u8]) -> u32 {
    let mut s: u64 = 0;
    for c in data.chunks(2) {
        s += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u64;
    }
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    s as u32
}

/// A TCP frame (IPv4 or IPv6, maybe VLAN-tagged) with a GSO header.
fn tcp_frame(rng: &mut Rng, v6: bool, vlan: bool, payload: usize, mss: usize) -> (Vec<u8>, [u8; 12]) {
    let mut f = vec![0x02, 0, 0, 0, 0, 1, 0x02, 0, 0, 0, 0, 2];
    if vlan {
        f.extend_from_slice(&[0x81, 0x00, 0x00, 0x05]);
    }
    if v6 {
        f.extend_from_slice(&[0x86, 0xdd, 0x60, 0, 0, 0, 0, 0, 6, 64]);
        for _ in 0..32 {
            f.push(rng.next() as u8);
        }
    } else {
        f.extend_from_slice(&[0x08, 0x00, 0x45, 0, 0, 0, 0x12, 0x34, 0x40, 0, 64, 6, 0, 0]);
        for _ in 0..8 {
            f.push(rng.next() as u8);
        }
    }
    let tcp = f.len();
    let seq = rng.next() as u32;
    f.extend_from_slice(&[0x1f, 0x90, 0xc3, 0x50]);
    f.extend_from_slice(&seq.to_be_bytes());
    f.extend_from_slice(&[0, 0, 0, 1, 0x50, 0x19 | 0x80, 0xff, 0xff, 0, 0, 0, 0]);
    for _ in 0..payload {
        f.push(rng.next() as u8);
    }
    let mut h = [0u8; 12];
    h[0] = offload::F_NEEDS_CSUM;
    h[1] = if v6 { offload::GSO_TCPV6 } else { offload::GSO_TCPV4 };
    h[4..6].copy_from_slice(&(mss as u16).to_le_bytes());
    h[6..8].copy_from_slice(&(tcp as u16).to_le_bytes());
    h[8..10].copy_from_slice(&16u16.to_le_bytes());
    (f, h)
}

/// netd's offload code: random headers and frames must not panic, and
/// segmentation of valid GSO frames must produce correct segments.
fn offload_check(rng: &mut Rng, n: u64) -> u64 {
    let mut buf = vec![0u8; 70000];
    let mut segs = 0u64;
    for _ in 0..n {
        // Garbage.
        let len = rng.below(2000) as usize;
        let mut frame: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let mut hdr: [u8; 12] = core::array::from_fn(|_| rng.next() as u8);
        if offload::sanitize(&mut hdr, &frame) {
            if offload::is_gso(&hdr) {
                offload::segment(&frame, &hdr, &mut buf, |f, _| assert!(f.len() <= frame.len() + 1500));
            }
            offload::complete_csum(&mut frame, &mut hdr);
        }
        // Valid GSO frames.
        let (v6, vlan) = (rng.below(2) == 0, rng.below(4) == 0);
        let mss = 500 + rng.below(1000) as usize;
        let payload = rng.below(20000) as usize;
        let (frame, mut hdr) = tcp_frame(rng, v6, vlan, payload, mss);
        assert!(offload::sanitize(&mut hdr, &frame), "valid GSO frame refused");
        let t = offload::parse_tcp(&frame).unwrap();
        let mut got = Vec::new();
        let mut count = 0;
        offload::segment(&frame, &hdr, &mut buf, |f, h| {
            assert_eq!(h[1], offload::GSO_NONE);
            assert!(f.len() - t.hdrs <= mss);
            // TCP checksum: pseudo header + segment sums to 0xffff.
            let tcp_len = f.len() - t.tcp;
            let pseudo = if v6 { csum(&f[t.ip + 8..t.ip + 40]) } else { csum(&f[t.ip + 12..t.ip + 20]) };
            let total = pseudo + 6 + tcp_len as u32 + csum(&f[t.tcp..]);
            let mut s = total;
            while s >> 16 != 0 {
                s = (s & 0xffff) + (s >> 16);
            }
            assert_eq!(s, 0xffff, "bad TCP checksum");
            if !v6 {
                assert_eq!(csum(&f[t.ip..t.tcp]), 0xffff, "bad IPv4 header checksum");
                assert_eq!(u16::from_be_bytes([f[t.ip + 2], f[t.ip + 3]]) as usize, f.len() - t.ip);
            } else {
                assert_eq!(u16::from_be_bytes([f[t.ip + 4], f[t.ip + 5]]) as usize, tcp_len);
            }
            let seq = u32::from_be_bytes(f[t.tcp + 4..t.tcp + 8].try_into().unwrap());
            let seq0 = u32::from_be_bytes(frame[t.tcp + 4..t.tcp + 8].try_into().unwrap());
            assert_eq!(seq.wrapping_sub(seq0) as usize, got.len());
            got.extend_from_slice(&f[t.hdrs..]);
            count += 1;
        });
        assert_eq!(got, frame[t.hdrs..], "segments do not add up to the payload");
        segs += count;
    }
    segs
}
