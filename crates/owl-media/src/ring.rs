//! A single-producer single-consumer float ring, written by the audio
//! decode thread and read by the sound card's callback.
//!
//! This is hand-rolled rather than a `Mutex<VecDeque<f32>>` for one
//! reason: the consumer is a realtime thread owned by the audio server.
//! If it ever blocks on a lock held by a decode thread that has just been
//! descheduled, the device underruns and the user hears a click. Two
//! atomics and a `Release`/`Acquire` pair cost nothing and cannot block.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub struct AudioRing {
    buf: UnsafeCell<Box<[f32]>>,
    cap: usize,
    /// Monotonic sample counters, never wrapped by hand: the difference is
    /// the fill level and the modulo is the index. usize is 64-bit here,
    /// so at 192 kHz this overflows after about three million years.
    read: AtomicUsize,
    write: AtomicUsize,
    /// Total samples the device has taken since the last `reset`. This is
    /// what the clock is derived from, and it is exact: the audio stream
    /// is continuous from the seek point, so elapsed time is just a
    /// division. See `clock.rs`.
    consumed: AtomicU64,
}

// Safety: `buf` is only ever touched through `read`/`write`, and the two
// counters fence every access. The producer writes strictly ahead of the
// consumer and neither ever reads the other's live region.
unsafe impl Send for AudioRing {}
unsafe impl Sync for AudioRing {}

impl AudioRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: UnsafeCell::new(vec![0.0; capacity].into_boxed_slice()),
            cap: capacity,
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            consumed: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Samples waiting to be played.
    pub fn len(&self) -> usize {
        self.write.load(Ordering::Acquire) - self.read.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn free(&self) -> usize {
        self.cap - self.len()
    }

    /// Producer side. Returns how many samples were taken; a short write
    /// means the ring is full and the decode thread should wait rather
    /// than spin, because the device is the thing setting the pace.
    pub fn push(&self, src: &[f32]) -> usize {
        let write = self.write.load(Ordering::Relaxed);
        let free = self.cap - (write - self.read.load(Ordering::Acquire));
        let n = src.len().min(free);
        let buf = unsafe { &mut *self.buf.get() };
        for (i, &s) in src[..n].iter().enumerate() {
            buf[(write + i) % self.cap] = s;
        }
        self.write.store(write + n, Ordering::Release);
        n
    }

    /// Consumer side, called from the realtime callback. Anything the ring
    /// could not supply is filled with silence: a gap is far less
    /// objectionable than whatever was left in the device buffer.
    pub fn pop_into(&self, dst: &mut [f32]) -> usize {
        let read = self.read.load(Ordering::Relaxed);
        let avail = self.write.load(Ordering::Acquire) - read;
        let n = dst.len().min(avail);
        let buf = unsafe { &*self.buf.get() };
        for (i, d) in dst[..n].iter_mut().enumerate() {
            *d = buf[(read + i) % self.cap];
        }
        dst[n..].fill(0.0);
        self.read.store(read + n, Ordering::Release);
        self.consumed.fetch_add(n as u64, Ordering::Relaxed);
        n
    }

    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Relaxed)
    }

    /// Drop everything queued. Called on seek, where the buffered audio
    /// belongs to the old position and must not be heard.
    pub fn reset(&self) {
        let write = self.write.load(Ordering::Acquire);
        self.read.store(write, Ordering::Release);
        self.consumed.store(0, Ordering::Relaxed);
    }
}
