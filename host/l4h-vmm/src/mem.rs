//! The guest's I/O pool as seen by l4h-vmm: the only guest-physical memory
//! the VMM can reach.  Every address a virtio device uses (rings,
//! descriptor tables, buffers) must lie in it; guest RAM is never mapped.
//! Kept free of seL4 dependencies so that tests/fuzz-virtq can use it
//! unchanged.

pub struct GuestMem {
    /// Where the pool is mapped.
    pub base: usize,
    /// Guest-physical address of the pool.
    pub gpa: u64,
    pub size: u64,
}

impl GuestMem {
    /// Is guest-physical [gpa, gpa + len) inside the pool?
    pub fn valid(&self, gpa: u64, len: u64) -> bool {
        gpa.checked_sub(self.gpa)
            .and_then(|off| off.checked_add(len))
            .is_some_and(|end| end <= self.size)
    }

    fn ptr<T>(&self, gpa: u64) -> *mut T {
        assert!(self.valid(gpa, core::mem::size_of::<T>() as u64), "guest access outside the I/O pool: {:#x}", gpa);
        (self.base + (gpa - self.gpa) as usize) as *mut T
    }

    /// Read a value the guest may change concurrently.  Callers validate
    /// addresses that come from the guest; this panics on bad ones only as
    /// a last line of defence.  The guest chooses the alignment, so
    /// unaligned values are read byte by byte.  Only for plain integer
    /// types (any bit pattern is valid).
    pub fn read<T: Copy>(&self, gpa: u64) -> T {
        let p = self.ptr::<T>(gpa);
        if p.is_aligned() {
            return unsafe { p.read_volatile() };
        }
        let mut v = core::mem::MaybeUninit::<T>::uninit();
        let (src, dst) = (p as *const u8, v.as_mut_ptr() as *mut u8);
        for i in 0..core::mem::size_of::<T>() {
            unsafe { dst.add(i).write(src.add(i).read_volatile()) };
        }
        unsafe { v.assume_init() }
    }

    /// Pointer to `len` bytes of the pool, which must be valid.
    pub fn bytes(&self, gpa: u64, len: usize) -> *mut u8 {
        assert!(self.valid(gpa, len as u64), "guest access outside the I/O pool: {:#x}+{:#x}", gpa, len);
        (self.base + (gpa - self.gpa) as usize) as *mut u8
    }

    pub fn write<T: Copy>(&self, gpa: u64, v: T) {
        let p = self.ptr::<T>(gpa);
        if p.is_aligned() {
            return unsafe { p.write_volatile(v) };
        }
        let (src, dst) = (&v as *const T as *const u8, p as *mut u8);
        for i in 0..core::mem::size_of::<T>() {
            unsafe { dst.add(i).write_volatile(src.add(i).read()) };
        }
    }
}
