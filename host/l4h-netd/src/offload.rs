//! virtio-net offload metadata (struct virtio_net_hdr_v1) on frames that
//! cross the switch: validation of what a guest sent, and TCP segmentation
//! for receivers that cannot take large (GSO) frames.  Free of seL4
//! dependencies so that it can be tested on the build machine.

pub const HDR_SIZE: usize = 12;
pub const F_NEEDS_CSUM: u8 = 1;
pub const GSO_NONE: u8 = 0;
pub const GSO_TCPV4: u8 = 1;
pub const GSO_TCPV6: u8 = 4;
pub const GSO_ECN: u8 = 0x80;

fn get16(h: &[u8; HDR_SIZE], off: usize) -> usize {
    u16::from_le_bytes([h[off], h[off + 1]]) as usize
}

fn put16(h: &mut [u8; HDR_SIZE], off: usize, v: usize) {
    h[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

/// Headers of a TCP segment: offsets of the IP and TCP headers and the
/// length of all headers.
#[derive(Clone, Copy, Debug)]
pub struct TcpFrame {
    pub ip: usize,
    pub v6: bool,
    pub tcp: usize,
    pub hdrs: usize,
}

/// Parse Ethernet (optionally one 802.1Q tag) + IPv4/IPv6 + TCP, without
/// IP options beyond the header length or IPv6 extension headers.
pub fn parse_tcp(frame: &[u8]) -> Option<TcpFrame> {
    let mut ip = 14;
    let mut ethertype = u16::from_be_bytes([*frame.get(12)?, *frame.get(13)?]);
    if ethertype == 0x8100 {
        ethertype = u16::from_be_bytes([*frame.get(16)?, *frame.get(17)?]);
        ip = 18;
    }
    let (v6, tcp) = match ethertype {
        0x0800 => {
            let ihl = (*frame.get(ip)? & 0x0f) as usize * 4;
            if *frame.get(ip)? >> 4 != 4 || ihl < 20 || *frame.get(ip + 9)? != 6 {
                return None;
            }
            (false, ip + ihl)
        }
        0x86dd => {
            if *frame.get(ip)? >> 4 != 6 || *frame.get(ip + 6)? != 6 {
                return None;
            }
            (true, ip + 40)
        }
        _ => return None,
    };
    let thl = (*frame.get(tcp + 12)? >> 4) as usize * 4;
    if thl < 20 || tcp + thl > frame.len() {
        return None;
    }
    Some(TcpFrame { ip, v6, tcp, hdrs: tcp + thl })
}

/// Check and normalise the header a guest sent with `frame`.  Returns
/// false if the frame must be dropped.
pub fn sanitize(hdr: &mut [u8; HDR_SIZE], frame: &[u8]) -> bool {
    // Only NEEDS_CSUM has a meaning from driver to device.
    hdr[0] &= F_NEEDS_CSUM;
    hdr[10] = 0;
    hdr[11] = 0;
    if hdr[0] & F_NEEDS_CSUM != 0 && get16(hdr, 6) + get16(hdr, 8) + 2 > frame.len() {
        return false;
    }
    match hdr[1] & !GSO_ECN {
        GSO_NONE => {
            hdr[1] = GSO_NONE;
            put16(hdr, 2, 0);
            put16(hdr, 4, 0);
            true
        }
        t @ (GSO_TCPV4 | GSO_TCPV6) => {
            // The receiver trusts these: they must describe the frame.
            let Some(f) = parse_tcp(frame) else { return false };
            if f.v6 != (t == GSO_TCPV6) || hdr[0] & F_NEEDS_CSUM == 0 || get16(hdr, 4) == 0 {
                return false;
            }
            if get16(hdr, 6) != f.tcp || get16(hdr, 8) != 16 {
                return false;
            }
            put16(hdr, 2, f.hdrs);
            true
        }
        _ => false,
    }
}

pub fn is_gso(hdr: &[u8; HDR_SIZE]) -> bool {
    hdr[1] != GSO_NONE
}

/// Ones' complement sum of `data` (big-endian words) added to `sum`.
fn sum(mut sum: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
        if sum > 0xffff_0000 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
    }
    if let [b] = chunks.remainder() {
        sum += (*b as u32) << 8;
    }
    sum
}

fn fold(mut s: u32) -> u16 {
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    s as u16
}

/// Complete a checksum the sender left to the device (NEEDS_CSUM): the
/// ones' complement sum from csum_start, stored at csum_start +
/// csum_offset (which holds the pseudo-header sum already).
pub fn complete_csum(frame: &mut [u8], hdr: &mut [u8; HDR_SIZE]) {
    if hdr[0] & F_NEEDS_CSUM == 0 {
        return;
    }
    let start = get16(hdr, 6);
    let at = start + get16(hdr, 8);
    if at + 2 <= frame.len() && start < frame.len() {
        let c = !fold(sum(0, &frame[start..]));
        frame[at..at + 2].copy_from_slice(&c.to_be_bytes());
    }
    hdr[0] &= !F_NEEDS_CSUM;
}

/// Split a GSO TCP frame (validated by `sanitize`) into MSS-sized frames
/// with complete checksums, built in `buf`, and pass each to `emit`.
pub fn segment(frame: &[u8], hdr: &[u8; HDR_SIZE], buf: &mut [u8], mut emit: impl FnMut(&mut [u8], &mut [u8; HDR_SIZE])) {
    let Some(f) = parse_tcp(frame) else { return };
    let mss = get16(hdr, 4);
    let payload = &frame[f.hdrs..];
    if mss == 0 || f.hdrs + mss > buf.len() {
        return;
    }
    let seq0 = u32::from_be_bytes(frame[f.tcp + 4..f.tcp + 8].try_into().unwrap());
    let flags0 = frame[f.tcp + 13];
    let id0 = u16::from_be_bytes([frame[f.ip + 4], frame[f.ip + 5]]);
    let n = payload.len().div_ceil(mss).max(1);
    for i in 0..n {
        let chunk = &payload[(i * mss).min(payload.len())..((i + 1) * mss).min(payload.len())];
        let len = f.hdrs + chunk.len();
        let out = &mut buf[..len];
        out[..f.hdrs].copy_from_slice(&frame[..f.hdrs]);
        out[f.hdrs..].copy_from_slice(chunk);
        let tcp_len = len - f.tcp;
        // IP header: lengths, IPv4 id and header checksum.
        if f.v6 {
            out[f.ip + 4..f.ip + 6].copy_from_slice(&(tcp_len as u16).to_be_bytes());
        } else {
            out[f.ip + 2..f.ip + 4].copy_from_slice(&((len - f.ip) as u16).to_be_bytes());
            out[f.ip + 4..f.ip + 6].copy_from_slice(&id0.wrapping_add(i as u16).to_be_bytes());
            out[f.ip + 10..f.ip + 12].copy_from_slice(&[0, 0]);
            let c = !fold(sum(0, &out[f.ip..f.tcp]));
            out[f.ip + 10..f.ip + 12].copy_from_slice(&c.to_be_bytes());
        }
        // TCP: sequence number; FIN and PSH only on the last segment, CWR
        // only on the first.
        let seq = seq0.wrapping_add((i * mss) as u32);
        out[f.tcp + 4..f.tcp + 8].copy_from_slice(&seq.to_be_bytes());
        let mut flags = flags0;
        if i + 1 < n {
            flags &= !0x09;
        }
        if i > 0 {
            flags &= !0x80;
        }
        out[f.tcp + 13] = flags;
        // TCP checksum over the pseudo header and the segment.
        out[f.tcp + 16..f.tcp + 18].copy_from_slice(&[0, 0]);
        let mut s = if f.v6 {
            sum(0, &out[f.ip + 8..f.ip + 40])
        } else {
            sum(0, &out[f.ip + 12..f.ip + 20])
        };
        s = sum(s, &[0, 6]);
        s += tcp_len as u32;
        let c = !fold(sum(s, &out[f.tcp..]));
        out[f.tcp + 16..f.tcp + 18].copy_from_slice(&c.to_be_bytes());
        let mut h = [0u8; HDR_SIZE];
        emit(out, &mut h);
    }
}
