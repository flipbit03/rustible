//! A test-only global allocator that looks at every buffer freed while a
//! test asks it to, and counts the ones that still hold a marker: the way to
//! show that bytes meant to be wiped (a secret, a file's chunk) were not left
//! behind in freed memory. Reading memory after it is freed is undefined
//! behaviour, so the look happens inside `dealloc`, before the buffer goes
//! back to the system allocator.
//!
//! `realloc` is left to `GlobalAlloc`'s default, which allocates the new
//! buffer, copies, and frees the old one through `dealloc`, so a `Vec` that
//! grows is seen too.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Sixteen bytes nothing else in the test binary holds. A test's data is
/// this, repeated.
pub(crate) const MARKER: &[u8; 16] = b"\xa5zeroize-probe\x5a\xc3";

/// The first 64 characters of the base64 of [`MARKER`] repeated: what a
/// frame carrying that data holds, as text.
const MARKER_B64: &[u8; 64] = b"pXplcm9pemUtcHJvYmVaw6V6ZXJvaXplLXByb2JlWsOlemVyb2l6ZS1wcm9iZVrD";

static WATCH: AtomicBool = AtomicBool::new(false);
static HITS: AtomicUsize = AtomicUsize::new(0);
/// One probe at a time: the count is global ([`exclusive`]).
static ONE: Mutex<()> = Mutex::new(());

struct Probe;

fn holds(buf: &[u8], needle: &[u8]) -> bool {
    buf.windows(needle.len()).any(|w| w == needle)
}

// SAFETY: every call goes to `System` unchanged; `dealloc` only reads the
// buffer it is handed, which is still allocated and `layout.size()` long.
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if WATCH.load(Ordering::SeqCst) {
            let buf = unsafe { std::slice::from_raw_parts(ptr, layout.size()) };
            if holds(buf, MARKER) || holds(buf, MARKER_B64) {
                HITS.fetch_add(1, Ordering::SeqCst);
            }
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static PROBE: Probe = Probe;

/// `n` bytes of [`MARKER`], repeated.
pub(crate) fn marked(n: usize) -> Vec<u8> {
    MARKER.iter().copied().cycle().take(n).collect()
}

/// The probe, held for the whole of a test that uses it: one such test at a
/// time, so the marker data another one makes or drops is never counted
/// here. Take it before making any marker data.
pub(crate) struct Exclusive(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

/// Wait for the probe.
pub(crate) fn exclusive() -> Exclusive {
    Exclusive(
        ONE.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

impl Exclusive {
    /// How many buffers were freed while `f` ran still holding the marker,
    /// or its base64, anywhere in the process.
    pub(crate) fn unwiped_frees(&self, f: impl FnOnce()) -> usize {
        HITS.store(0, Ordering::SeqCst);
        WATCH.store(true, Ordering::SeqCst);
        f();
        WATCH.store(false, Ordering::SeqCst);
        HITS.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe sees a buffer freed with the marker in it, and one grown
    /// by `Vec`, and not one wiped first.
    #[test]
    fn the_probe_counts_what_was_not_wiped() {
        let probe = exclusive();
        assert_eq!(probe.unwiped_frees(|| drop(marked(64))), 1);
        let grown = probe.unwiped_frees(|| {
            let mut v = Vec::with_capacity(16);
            for _ in 0..8 {
                v.extend_from_slice(MARKER);
            }
            zeroize::Zeroize::zeroize(&mut v);
        });
        assert!(grown >= 2, "{grown}");
        assert_eq!(
            probe.unwiped_frees(|| drop(zeroize::Zeroizing::new(marked(64)))),
            0
        );
    }
}
