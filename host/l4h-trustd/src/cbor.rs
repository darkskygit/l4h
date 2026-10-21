//! A CBOR encoder (RFC 8949) for the few types reports use, in
//! deterministic encoding: shortest heads, definite lengths, and map keys
//! written in their sorted order by the callers.

use alloc::vec::Vec;

pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn new() -> Self {
        Enc(Vec::new())
    }

    fn head(&mut self, major: u8, v: u64) -> &mut Self {
        let m = major << 5;
        match v {
            0..=23 => self.0.push(m | v as u8),
            24..=0xff => self.0.extend_from_slice(&[m | 24, v as u8]),
            0x100..=0xffff => {
                self.0.push(m | 25);
                self.0.extend_from_slice(&(v as u16).to_be_bytes());
            }
            0x1_0000..=0xffff_ffff => {
                self.0.push(m | 26);
                self.0.extend_from_slice(&(v as u32).to_be_bytes());
            }
            _ => {
                self.0.push(m | 27);
                self.0.extend_from_slice(&v.to_be_bytes());
            }
        }
        self
    }

    pub fn uint(&mut self, v: u64) -> &mut Self {
        self.head(0, v)
    }

    /// A negative integer: -1 - n.
    pub fn nint(&mut self, n: u64) -> &mut Self {
        self.head(1, n)
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.head(2, b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }

    pub fn text(&mut self, s: &str) -> &mut Self {
        self.head(3, s.len() as u64);
        self.0.extend_from_slice(s.as_bytes());
        self
    }

    pub fn array(&mut self, n: usize) -> &mut Self {
        self.head(4, n as u64)
    }

    pub fn map(&mut self, n: usize) -> &mut Self {
        self.head(5, n as u64)
    }

    pub fn tag(&mut self, t: u64) -> &mut Self {
        self.head(6, t)
    }

    pub fn bool(&mut self, b: bool) -> &mut Self {
        self.0.push(if b { 0xf5 } else { 0xf4 });
        self
    }
}
