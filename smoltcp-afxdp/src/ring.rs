//! Single-producer / single-consumer rings shared with the kernel via mmap.
//!
//! The fill and TX rings are produced by userspace; the RX and completion
//! rings are produced by the kernel. Index updates are cached locally and
//! published in batches (`submit` / `release`).

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Ring<T: Copy> {
    producer: *const AtomicU32,
    consumer: *const AtomicU32,
    flags: *const AtomicU32,
    descs: *mut T,
    mask: u32,
    size: u32,
    cached_prod: u32,
    cached_cons: u32,
    map: *mut libc::c_void,
    map_len: usize,
}

impl<T: Copy> Ring<T> {
    /// Map a ring whose layout is described by `off` at mmap page offset `pgoff`.
    pub(crate) fn map(fd: RawFd, off: &libc::xdp_ring_offset, size: u32, pgoff: u64) -> io::Result<Self> {
        let map_len = off.desc as usize + size as usize * std::mem::size_of::<T>();
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                pgoff as libc::off_t,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let at = |o: u64| unsafe { map.cast::<u8>().add(o as usize) };
        let mut ring = Self {
            producer: at(off.producer).cast(),
            consumer: at(off.consumer).cast(),
            flags: at(off.flags).cast(),
            descs: at(off.desc).cast(),
            mask: size - 1,
            size,
            cached_prod: 0,
            cached_cons: 0,
            map,
            map_len,
        };
        ring.cached_prod = ring.prod().load(Ordering::Relaxed);
        ring.cached_cons = ring.cons().load(Ordering::Relaxed);
        Ok(ring)
    }

    fn prod(&self) -> &AtomicU32 {
        unsafe { &*self.producer }
    }
    fn cons(&self) -> &AtomicU32 {
        unsafe { &*self.consumer }
    }

    /// Kernel asks to be woken up (only meaningful with `XDP_USE_NEED_WAKEUP`).
    pub fn needs_wakeup(&self) -> bool {
        unsafe { &*self.flags }.load(Ordering::Relaxed) & libc::XDP_RING_NEED_WAKEUP != 0
    }

    // --- producer side (fill, tx) ---

    /// Number of slots userspace may still write.
    pub fn free(&mut self) -> u32 {
        let free = self.size - self.cached_prod.wrapping_sub(self.cached_cons);
        if free > 0 {
            return free;
        }
        self.cached_cons = self.cons().load(Ordering::Acquire);
        self.size - self.cached_prod.wrapping_sub(self.cached_cons)
    }

    /// Write one entry. Not visible to the kernel until `submit`.
    pub fn push(&mut self, v: T) -> bool {
        if self.free() == 0 {
            return false;
        }
        unsafe { self.descs.add((self.cached_prod & self.mask) as usize).write(v) };
        self.cached_prod = self.cached_prod.wrapping_add(1);
        true
    }

    pub fn submit(&self) {
        self.prod().store(self.cached_prod, Ordering::Release);
    }

    // --- consumer side (rx, completion) ---

    /// Read one entry. The slot is not handed back to the kernel until `release`.
    pub fn pop(&mut self) -> Option<T> {
        if self.cached_cons == self.cached_prod {
            self.cached_prod = self.prod().load(Ordering::Acquire);
            if self.cached_cons == self.cached_prod {
                return None;
            }
        }
        let v = unsafe { self.descs.add((self.cached_cons & self.mask) as usize).read() };
        self.cached_cons = self.cached_cons.wrapping_add(1);
        Some(v)
    }

    pub fn release(&self) {
        self.cons().store(self.cached_cons, Ordering::Release);
    }
}

impl<T: Copy> Drop for Ring<T> {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.map, self.map_len) };
    }
}
