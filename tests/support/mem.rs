//! Peak-heap measurement for memory-bound tests (NFR-3).
//!
//! The including test binary must declare
//! `#[global_allocator] static ALLOC: dhat::Alloc = dhat::Alloc;`.

/// Runs `f` and returns its result with the peak heap bytes allocated meanwhile.
pub fn peak_heap<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let _profiler = dhat::Profiler::builder().testing().build();
    let value = f();
    (value, dhat::HeapStats::get().max_bytes)
}
