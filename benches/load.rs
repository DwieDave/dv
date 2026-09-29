//! Load benchmarks over the fixtures from `just data`.
//!
//! Reading the file is the I/O baseline that parse benchmarks are compared against.

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dv::index::spill::{SpillBuilder, SpillLimits, SpillStore};
use dv::json::parse::parse;
use dv::json::stream::{StreamLimits, parse_stream};
use dv::load::{Request, load};
use dv::source::file::FileSource;
use std::ops::ControlFlow;
use std::sync::atomic::AtomicBool;

const FIXTURES: [&str; 8] = [
    "api-15M.json",
    "dense-15M.json",
    "small-objects-15M.json",
    "wide-15M.json",
    "escapes-15M.json",
    "deep-100k.json",
    "ndjson-15M.ndjson",
    "yaml-15M.yaml",
];

fn fixture(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from("target/bench-data").join(name);
    path.exists().then_some(path)
}

fn loaded(names: &[&'static str]) -> Vec<(&'static str, Vec<u8>)> {
    let load = |name: &'static str| {
        let bytes = fixture(name).and_then(|path| fs::read(path).ok());
        if bytes.is_none() {
            eprintln!("missing fixture {name}: run `just data`");
        }
        bytes.map(|bytes| (name, bytes))
    };
    names.iter().filter_map(|&name| load(name)).collect()
}

fn read_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("read");
    for name in FIXTURES {
        let Some(path) = fixture(name) else {
            continue;
        };
        let len = fs::metadata(&path).map_or(0, |m| m.len());
        group.throughput(Throughput::Bytes(len));
        group.bench_function(name, |b| b.iter(|| fs::read(black_box(&path))));
    }
    group.finish();
}

fn parse_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");
    for (name, bytes) in loaded(&FIXTURES[..6]) {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_function(name, |b| b.iter(|| parse(black_box(&bytes))));
    }
    group.finish();
}

fn load_formats(c: &mut Criterion) {
    let mut group = c.benchmark_group("load");
    group.sample_size(10);
    for (name, bytes) in loaded(&FIXTURES[6..]) {
        let request = Request {
            path: fixture(name),
            max_len: u64::MAX,
            ..Request::default()
        };
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                load(
                    black_box(bytes.as_slice()),
                    &request,
                    &mut drop,
                    &AtomicBool::new(false),
                );
            });
        });
    }
    group.finish();
}

/// Streams and spills one index of `path`; `None` on any failure.
fn stream_once(path: &std::path::Path) -> Option<SpillStore> {
    let source = FileSource::new(fs::File::open(path).ok()?, 64 << 20).ok()?;
    let builder = SpillBuilder::new(SpillLimits::default()).ok()?;
    let hook = |_| ControlFlow::Continue(());
    let parsed = parse_stream(
        &source,
        builder,
        StreamLimits::default(),
        hook,
        |_, _, _| {},
    )
    .ok()?;
    parsed.builder.finish().ok()
}

fn stream_index(c: &mut Criterion) {
    let Some(path) = fixture("api-100M.json") else {
        return;
    };
    let len = fs::metadata(&path).map_or(0, |m| m.len());
    let mut group = c.benchmark_group("stream");
    group.sample_size(10).throughput(Throughput::Bytes(len));
    group.bench_function("api-100M.json", |b| {
        b.iter(|| stream_once(&path));
    });
    group.finish();
}

criterion_group!(
    benches,
    read_baseline,
    parse_json,
    load_formats,
    stream_index
);
criterion_main!(benches);
