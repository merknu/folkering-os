//! Lock-free SPSC byte ring for IPC display lists.
//!
//! Producer (app) and consumer (compositor) each see the same backing region
//! mapped at different virtual addresses. Head/tail are atomic counters
//! padded to separate cache lines so producer/consumer updates don't trigger
//! false sharing on x86_64 (64-byte cache lines).
//!
//! ## Layout
//!
//! ```text
//! offset   field
//! ------   ----------------------------------------
//! 0        head: AtomicUsize    (producer-owned)
//! 8        _pad1: [u8; 56]      (cache-line padding)
//! 64       tail: AtomicUsize    (consumer-owned)
//! 72       _pad2: [u8; 56]      (cache-line padding)
//! 128      buffer: [u8; CAPACITY]
//! ```
//!
//! Both `head` and `tail` are byte indices into `buffer`, monotonically
//! increasing. A reader/writer wraps with `idx % CAPACITY` when accessing
//! the underlying buffer. CAPACITY must be a power of two so the wrap is a
//! cheap mask, but we use `%` here for clarity — any reasonable optimizer
//! turns it into the mask form for power-of-two constants.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Capacity in bytes. 64 KiB is enough for several frames worth of display
/// lists at typical UI complexity (~1-2 KiB/frame). Power of two so the
/// modulo turns into a mask. Must match what the compositor maps.
pub const RING_CAPACITY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// Consumer hasn't drained enough room for this list. Caller should drop
    /// the frame or retry next tick — never spin: that'd starve the consumer.
    Full,
    /// Display list exceeds total ring capacity. Caller bug.
    TooLarge,
}

/// Backing layout for the shared ring. The producer constructs an
/// `IpcGraphicsRing<RING_CAPACITY_BYTES>` over the shared region; the
/// consumer constructs the same type over its mapping of the same pages.
///
/// `Send`/`Sync` are *not* derived because the type contains an
/// `UnsafeCell`. Callers are responsible for ensuring exactly one producer
/// and exactly one consumer touch the ring.
#[repr(C, align(64))]
pub struct IpcGraphicsRing<const N: usize> {
    pub head: AtomicUsize,
    _pad1: [u8; 56],
    pub tail: AtomicUsize,
    _pad2: [u8; 56],
    buffer: UnsafeCell<[u8; N]>,
}

// SAFETY: the type is designed to be shared across producer/consumer
// processes via shmem. The synchronization is the SPSC discipline, not Rust's
// borrow checker — it's the caller's job to enforce single-producer
// single-consumer.
unsafe impl<const N: usize> Sync for IpcGraphicsRing<N> {}

impl<const N: usize> IpcGraphicsRing<N> {
    /// Initialize an empty ring in place. Called once by the kernel-side
    /// allocator (or in tests) on freshly-zeroed memory.
    pub const fn new() -> Self {
        Self {
            head: AtomicUsize::new(0),
            _pad1: [0; 56],
            tail: AtomicUsize::new(0),
            _pad2: [0; 56],
            buffer: UnsafeCell::new([0; N]),
        }
    }

    /// Bytes currently in the ring (producer view).
    #[inline]
    pub fn occupied(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        head.wrapping_sub(tail)
    }

    /// Bytes the producer can push without blocking.
    #[inline]
    pub fn capacity_remaining(&self) -> usize {
        N - self.occupied()
    }

    /// Producer: append `data` to the ring. Returns `Err(Full)` if the
    /// consumer hasn't drained enough room. The write happens with two
    /// `copy_nonoverlapping`s when the slice straddles the wrap point.
    pub fn push(&self, data: &[u8]) -> Result<(), PushError> {
        if data.len() > N {
            return Err(PushError::TooLarge);
        }
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let occupied = head.wrapping_sub(tail);
        if occupied + data.len() > N {
            return Err(PushError::Full);
        }

        let buf_ptr = self.buffer.get() as *mut u8;
        let write_off = head % N;
        let first = core::cmp::min(data.len(), N - write_off);

        // SAFETY: `write_off + first <= N` and `data.len() - first <= write_off`.
        // The wrap-second case writes at offset 0, well inside `buffer`.
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), buf_ptr.add(write_off), first);
            if first < data.len() {
                core::ptr::copy_nonoverlapping(
                    data.as_ptr().add(first),
                    buf_ptr,
                    data.len() - first,
                );
            }
        }

        // Release: consumer's Acquire-load of `head` will see all the bytes
        // we just wrote.
        self.head.store(head.wrapping_add(data.len()), Ordering::Release);
        Ok(())
    }

    /// Consumer: copy at most `out.len()` bytes from the ring into `out` and
    /// advance `tail`. Returns the number of bytes consumed. This is
    /// destructive — bytes are released back to the producer.
    pub fn pop_into(&self, out: &mut [u8]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail);
        let n = core::cmp::min(avail, out.len());
        if n == 0 {
            return 0;
        }

        let buf_ptr = self.buffer.get() as *const u8;
        let read_off = tail % N;
        let first = core::cmp::min(n, N - read_off);

        // SAFETY: same bounds analysis as `push`. `read_off + first <= N`.
        unsafe {
            core::ptr::copy_nonoverlapping(buf_ptr.add(read_off), out.as_mut_ptr(), first);
            if first < n {
                core::ptr::copy_nonoverlapping(buf_ptr, out.as_mut_ptr().add(first), n - first);
            }
        }

        self.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Consumer: peek the next `out.len()` bytes without advancing `tail`.
    /// Useful for parsing variable-length records where we need to look at a
    /// header to decide how many bytes to consume.
    pub fn peek(&self, out: &mut [u8]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail);
        let n = core::cmp::min(avail, out.len());
        if n == 0 {
            return 0;
        }

        let buf_ptr = self.buffer.get() as *const u8;
        let read_off = tail % N;
        let first = core::cmp::min(n, N - read_off);

        unsafe {
            core::ptr::copy_nonoverlapping(buf_ptr.add(read_off), out.as_mut_ptr(), first);
            if first < n {
                core::ptr::copy_nonoverlapping(buf_ptr, out.as_mut_ptr().add(first), n - first);
            }
        }
        n
    }

    /// Consumer: drop `n` bytes from the front of the ring. Pairs with
    /// `peek` for the read-then-commit pattern.
    pub fn drop_n(&self, n: usize) {
        let tail = self.tail.load(Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(n), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_pop_roundtrip() {
        let ring: IpcGraphicsRing<128> = IpcGraphicsRing::new();
        let data = [1u8, 2, 3, 4, 5];
        ring.push(&data).unwrap();
        let mut out = [0u8; 5];
        assert_eq!(ring.pop_into(&mut out), 5);
        assert_eq!(out, data);
        assert_eq!(ring.occupied(), 0);
    }

    #[test]
    fn push_full_returns_err() {
        let ring: IpcGraphicsRing<8> = IpcGraphicsRing::new();
        ring.push(&[0u8; 6]).unwrap();
        assert_eq!(ring.push(&[7u8; 4]), Err(PushError::Full));
    }

    #[test]
    fn wrap_works() {
        let ring: IpcGraphicsRing<8> = IpcGraphicsRing::new();
        ring.push(&[1, 2, 3, 4, 5, 6]).unwrap();
        let mut out = [0u8; 5];
        assert_eq!(ring.pop_into(&mut out), 5); // tail = 5
        ring.push(&[7, 8, 9, 10, 11]).unwrap();   // wraps: 3 bytes after offset 6, then 2 at offset 0
        let mut out2 = [0u8; 6];
        assert_eq!(ring.pop_into(&mut out2), 6);
        assert_eq!(&out2, &[6, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn peek_does_not_advance() {
        let ring: IpcGraphicsRing<32> = IpcGraphicsRing::new();
        ring.push(&[1, 2, 3, 4]).unwrap();
        let mut p = [0u8; 2];
        assert_eq!(ring.peek(&mut p), 2);
        assert_eq!(&p, &[1, 2]);
        assert_eq!(ring.occupied(), 4);
        ring.drop_n(2);
        let mut rest = [0u8; 2];
        ring.pop_into(&mut rest);
        assert_eq!(&rest, &[3, 4]);
    }

    #[test]
    fn rejects_too_large() {
        let ring: IpcGraphicsRing<8> = IpcGraphicsRing::new();
        let big = [0u8; 9];
        assert_eq!(ring.push(&big), Err(PushError::TooLarge));
    }
}
