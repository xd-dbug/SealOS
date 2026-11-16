//! Keyboard input waiting to be read by a user program (`SYS_READ`).
//!
//! The keyboard ISR produces, a task in `syscall::sys_read` consumes. The
//! kernel no longer echoes keys itself: the reader (the shell) decides what
//! the screen shows, including how backspace and the prompt look.

use core::sync::atomic::{AtomicU64, Ordering};
use pc_keyboard::DecodedKey;
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;

/// Bytes the buffer holds. Fixed, because the producer is an interrupt handler
/// and `GuardedHeap` panics on any allocation there (design-review item 6), so
/// a growable `VecDeque` is out. 256 is several typed lines, far more than a
/// person gets ahead of a reader that polls every time slice.
pub const CAPACITY: usize = 256;

/// The longest thing pushed in one go: one `char` as UTF-8.
const MAX_KEY_BYTES: usize = 4;

/// Backspace as delivered to readers: ASCII BS, which is also what
/// `pc_keyboard` decodes the key to. (The Delete key arrives as `0x7f`.)
pub const BACKSPACE: u8 = 0x08;

/// A byte FIFO in a fixed array. Plain data with no locking of its own, so
/// the policy below can be read (and tested) on its own.
///
/// Full-buffer policy: **the newest input is dropped**, and dropped whole. An
/// overwrite-the-oldest ring would instead eat the *start* of what was typed,
/// turning `rm -r tmp` into `-r tmp`; losing the tail is the failure a typist
/// can see and retype. "Whole" matters for multi-byte keys: half of a UTF-8
/// sequence in the buffer would hand the reader invalid text.
struct Ring {
    bytes: [u8; CAPACITY],
    /// Index of the oldest byte.
    head: usize,
    len: usize,
}

impl Ring {
    const fn new() -> Self {
        Ring { bytes: [0; CAPACITY], head: 0, len: 0 }
    }

    /// Appends all of `bytes`, or none of them if they don't all fit. Never
    /// waits for room: the caller may be an ISR.
    fn push_all(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > CAPACITY - self.len {
            return false;
        }
        for &byte in bytes {
            self.bytes[(self.head + self.len) % CAPACITY] = byte;
            self.len += 1;
        }
        true
    }

    /// Moves up to `out.len()` of the oldest bytes into `out`, returning how
    /// many. `len` is the only source of truth for what is readable, so an
    /// empty ring yields 0 and never a byte left over from an earlier lap.
    fn pop_into(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        for slot in &mut out[..n] {
            *slot = self.bytes[self.head];
            self.head = (self.head + 1) % CAPACITY;
        }
        self.len -= n;
        n
    }
}

/// A `Mutex` rather than a lock-free single-producer/single-consumer ring:
/// there are two producers (the keyboard ISR and `inject`, which runs in
/// mainline), so the SPSC precondition doesn't hold, and whole-key pushes
/// want to check the room and write under one critical section anyway.
///
/// The ISR takes this lock, so the rule is the one `SCHEDULER` and `PICS`
/// follow: mainline holds it only with IF=0. Every access goes through the
/// functions below, which all wrap it in `without_interrupts`; a keystroke
/// can then never arrive while mainline holds it, and the ISR never spins.
static INPUT: Mutex<Ring> = Mutex::new(Ring::new());

// Relaxed: a statistic, like `TICKS`.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// The one push path, shared by the ISR (`push_key`) and tests (`inject`).
fn push_all(bytes: &[u8]) -> bool {
    let pushed = without_interrupts(|| INPUT.lock().push_all(bytes));
    if !pushed {
        DROPPED.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    }
    pushed
}

/// The bytes a terminal program expects for `key`, if it has any:
/// Enter -> `\n`, Backspace -> `BACKSPACE`, any other Unicode key -> its
/// UTF-8 bytes. `pc_keyboard`'s US layout already decodes Enter and Backspace
/// to those characters, so the mapping is the UTF-8 encoding itself, except
/// that a `\r` (other layouts, or a future serial console) is folded into
/// `\n` so readers only ever see one line terminator.
///
/// Keys with no byte equivalent (arrows, F-keys, bare modifiers) are `None`.
/// They'd need escape sequences, which nothing reads yet.
fn key_bytes(key: DecodedKey, out: &mut [u8; MAX_KEY_BYTES]) -> Option<&[u8]> {
    match key {
        DecodedKey::Unicode('\r') => Some('\n'.encode_utf8(out).as_bytes()),
        DecodedKey::Unicode(c) => Some(c.encode_utf8(out).as_bytes()),
        DecodedKey::RawKey(_) => None,
    }
}

/// Queues a decoded key for readers. Called by the keyboard ISR; allocation
/// free and never blocks. False if the key has no bytes or the buffer was
/// full (the key is dropped, see `Ring`).
pub fn push_key(key: DecodedKey) -> bool {
    let mut encoded = [0; MAX_KEY_BYTES];
    match key_bytes(key, &mut encoded) {
        Some(bytes) => push_all(bytes),
        None => false,
    }
}

/// Test hook, in the spirit of `syscall::set_write_tap`: queues `bytes` as if
/// they had been typed, so a test can script input without a PS/2 device.
/// Same push path as the ISR, one byte at a time (each byte is its own
/// "key"), so the full-buffer policy applies per byte. Returns how many were
/// accepted; it stops at the first drop.
pub fn inject(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|&&byte| push_all(core::slice::from_ref(&byte)))
        .count()
}

/// Moves up to `out.len()` buffered bytes into `out`, oldest first, and
/// returns how many (0 if nothing is waiting). Never blocks; `sys_read` does
/// the waiting, outside this lock.
pub fn read(out: &mut [u8]) -> usize {
    without_interrupts(|| INPUT.lock().pop_into(out))
}

/// Bytes waiting to be read.
pub fn len() -> usize {
    without_interrupts(|| INPUT.lock().len)
}

/// Bytes dropped because the buffer was full, since boot.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Throws away everything buffered. For tests that need a known-empty start.
pub fn clear() {
    without_interrupts(|| {
        let mut ring = INPUT.lock();
        ring.head = 0;
        ring.len = 0;
    });
}
