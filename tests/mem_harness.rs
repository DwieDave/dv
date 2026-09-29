#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod support {
    pub mod mem;
}

use support::mem::peak_heap;

const MIB: usize = 1024 * 1024;

#[test]
fn peak_heap_reports_a_large_allocation() {
    let (buf, peak) = peak_heap(|| vec![1u8; 10 * MIB]);
    assert_eq!(buf.len(), 10 * MIB);
    assert!((10 * MIB..11 * MIB).contains(&peak), "peak {peak}");
}
