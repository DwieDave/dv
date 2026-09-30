//! JSON: streaming and in-memory parsing must agree on any input.
#![no_main]
#![forbid(unsafe_code)]

use std::ops::ControlFlow;

use dv::index::store::VecStoreBuilder;
use dv::json::parse::parse;
use dv::json::stream::{StreamLimits, parse_stream};
use dv::source::MemSource;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&window, bytes)) = data.split_first() else {
        return;
    };
    let limits = StreamLimits {
        initial: usize::from(window).max(1),
        max: 1 << 20,
    };
    let source = MemSource::new(bytes.to_vec());
    let go = |_| ControlFlow::Continue(());
    let streamed = parse_stream(
        &source,
        VecStoreBuilder::default(),
        limits,
        go,
        |_, _, _| {},
    );
    let memory = parse(bytes);
    assert_eq!(
        streamed
            .map(|p| (p.root, p.values))
            .map_err(|e| e.to_string()),
        memory
            .map(|p| (p.root, p.values))
            .map_err(|e| e.to_string())
    );
});
