//! A CBOR encoder (RFC 8949) for the few types reports use, in
//! deterministic encoding: shortest heads, definite lengths, and map keys
//! written in their sorted order by the callers.
//!
//! It writes into a buffer the caller sizes: what does not fit is not
//! written, and `done` says the encoding overflowed (the calls chain, so
//! the check is once at the end).

pub struct Enc<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflow: bool,
}

/// The encoding did not fit its buffer.
#[derive(Debug)]
pub struct Overflow;

impl<'a> Enc<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Enc { buf, len: 0, overflow: false }
    }

    /// The encoding, if it fitted.
    pub fn done(self) -> Result<&'a [u8], Overflow> {
        let Enc { buf, len, overflow } = self;
        if overflow {
            return Err(Overflow);
        }
        Ok(&buf[..len])
    }

    fn put(&mut self, b: &[u8]) -> &mut Self {
        match self.buf.get_mut(self.len..self.len + b.len()) {
            Some(dst) if !self.overflow => {
                dst.copy_from_slice(b);
                self.len += b.len();
            }
            _ => self.overflow = true,
        }
        self
    }

    fn head(&mut self, major: u8, v: u64) -> &mut Self {
        let m = major << 5;
        match v {
            0..=23 => self.put(&[m | v as u8]),
            24..=0xff => self.put(&[m | 24, v as u8]),
            0x100..=0xffff => self.put(&[m | 25]).put(&(v as u16).to_be_bytes()),
            0x1_0000..=0xffff_ffff => self.put(&[m | 26]).put(&(v as u32).to_be_bytes()),
            _ => self.put(&[m | 27]).put(&v.to_be_bytes()),
        }
    }

    pub fn uint(&mut self, v: u64) -> &mut Self {
        self.head(0, v)
    }

    /// A negative integer: -1 - n.
    pub fn nint(&mut self, n: u64) -> &mut Self {
        self.head(1, n)
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.head(2, b.len() as u64).put(b)
    }

    pub fn text(&mut self, s: &str) -> &mut Self {
        self.head(3, s.len() as u64).put(s.as_bytes())
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
        self.put(&[if b { 0xf5 } else { 0xf4 }])
    }
}
