//! NDJSON: streaming and in-memory line indexes must agree on any input.
#![no_main]
#![forbid(unsafe_code)]

use std::ops::ControlFlow;

use dv::index::lines::{LineSpill, Lines};
use dv::index::store::VecStoreBuilder;
use dv::json::lines_stream::parse_lines_stream;
use dv::json::ndjson::parse_lines;
use dv::json::stream::StreamLimits;
use dv::source::MemSource;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&window, bytes)) = data.split_first() else {
        return;
    };
    let go = |_| ControlFlow::Continue(());
    let Ok(memory) = parse_lines(bytes, go) else {
        return;
    };
    let limits = StreamLimits {
        initial: usize::from(window).max(1),
        max: 1 << 20,
    };
    let source = MemSource::new(bytes.to_vec());
    let Ok(spill) = LineSpill::new(4) else {
        return;
    };
    let streamed = parse_lines_stream(
        &source,
        VecStoreBuilder::default(),
        spill,
        limits,
        go,
        |_, _, _, _| {},
    );
    let streamed = streamed
        .map(|s| (s.lines.count(), s.values))
        .map_err(|e| e.to_string());
    assert_eq!(streamed, Ok((memory.lines.count(), memory.values)));
});
