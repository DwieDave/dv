//! YAML: transcoding never panics, and its output is always valid JSON (NFR-6).
#![no_main]
#![forbid(unsafe_code)]

use std::ops::ControlFlow;

use dv::json::parse::parse;
use dv::yaml::{budget, transcode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(out) = transcode(text, budget(text.len()), |_| ControlFlow::Continue(())) {
        assert!(parse(&out.json).is_ok());
    }
});
