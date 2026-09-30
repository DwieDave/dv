//! dv: a fast terminal viewer for large JSON, NDJSON and YAML files.
#![forbid(unsafe_code)]

pub mod app;
pub mod cli;
pub mod clipboard;
pub mod config;
pub mod document;
pub mod error;
pub mod filter;
pub mod format;
pub mod index;
pub mod json;
pub mod live_tree;
pub mod load;
pub mod mode;
pub mod path;
pub mod position;
pub mod pulse;
pub mod schema;
pub mod search;
pub mod snippet;
pub mod source;
pub mod state_file;
pub mod stream_core;
pub mod stream_tree;
pub mod temp;
pub mod tree;
pub mod ui;
pub mod view;
pub mod yaml;

#[cfg(test)]
mod test_support;
