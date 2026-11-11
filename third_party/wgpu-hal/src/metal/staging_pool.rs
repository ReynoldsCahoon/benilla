//! benilla: wgpu-core's staging buffers come from a pool (`BENILLA.md`, "Staging buffers pooled").
//!
//! wgpu-core makes a fresh `MAP_WRITE | COPY_SRC` buffer for every `write_buffer`,
//! `write_buffer_with` and `write_texture`, and destroys it once its submission completes. On iOS
//! a Metal buffer's creation and release are costly: on the iPad Air 2 a frame's `queue.submit`
//! spent most of its ~15 ms releasing them. A staging buffer reaches `destroy_buffer` only after
//! the GPU is done with it, so here it is kept, by power-of-two size class, and the next staging
//! buffer of that class reuses it. The pool holds at most [`CAP_BYTES`]; past that a returned
//! buffer is released as before.

use parking_lot::Mutex;

/// The smallest class: smaller requests take a 4 KiB buffer.
const MIN_CLASS: u64 = 4 << 10;
/// The largest pooled class; a bigger staging buffer is made and released as before.
const MAX_CLASS: u64 = 4 << 20;
/// The most the pool keeps.
const CAP_BYTES: u64 = 16 << 20;
/// Classes from `MIN_CLASS` to `MAX_CLASS`, one per power of two.
const CLASSES: usize = (MAX_CLASS.trailing_zeros() - MIN_CLASS.trailing_zeros() + 1) as usize;

/// The pool's switch, on by default; benilla sets it once at startup (`WOW_STAGING_POOL=0` turns it
/// off for a measurement).
pub static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

struct Pooled(metal::Buffer);
// SAFETY: a Metal buffer object is thread-safe to retain, release and hand between threads; the
// pool only stores idle buffers the GPU has finished with.
unsafe impl Send for Pooled {}

struct Pool {
    free: [alloc::vec::Vec<Pooled>; CLASSES],
    bytes: u64,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    free: [const { alloc::vec::Vec::new() }; CLASSES],
    bytes: 0,
});

/// Whether `desc` is a staging buffer the pool serves: write-mapped and a copy source, nothing else.
pub(super) fn is_staging(desc: &crate::BufferDescriptor) -> bool {
    ENABLED.load(core::sync::atomic::Ordering::Relaxed)
        && desc.usage == wgt::BufferUses::MAP_WRITE | wgt::BufferUses::COPY_SRC
}

/// The class a staging buffer of `size` bytes is allocated at, or `None` past [`MAX_CLASS`].
pub(super) fn class(size: u64) -> Option<u64> {
    let c = size.max(MIN_CLASS).checked_next_power_of_two()?;
    (c <= MAX_CLASS).then_some(c)
}

fn slot(class: u64) -> usize {
    (class.trailing_zeros() - MIN_CLASS.trailing_zeros()) as usize
}

/// An idle buffer of `size`'s class, if the pool has one.
pub(super) fn take(size: u64) -> Option<metal::Buffer> {
    let c = class(size)?;
    let mut pool = POOL.lock();
    let buffer = pool.free[slot(c)].pop()?;
    pool.bytes -= c;
    Some(buffer.0)
}

/// Keep `buffer` (a class-sized staging buffer the GPU is done with) for reuse, or release it if
/// the pool is full.
pub(super) fn give(buffer: metal::Buffer) {
    let c = buffer.length();
    let Some(cc) = class(c).filter(|&cc| cc == c) else {
        return;
    };
    let mut pool = POOL.lock();
    if pool.bytes + cc > CAP_BYTES {
        return; // dropped here: released as before
    }
    pool.bytes += cc;
    pool.free[slot(cc)].push(Pooled(buffer));
}
