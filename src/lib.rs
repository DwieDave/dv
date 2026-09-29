//! dv: a fast terminal viewer for large JSON, NDJSON and YAML files.
#![forbid(unsafe_code)]

pub mod app;
pub mod cli;
pub mod error;
pub mod format;
pub mod index;
pub mod json;
pub mod load;
pub mod path;
pub mod position;
pub mod snippet;
pub mod source;
pub mod tree;
pub mod ui;
pub mod view;
pub mod yaml;

#[cfg(test)]
mod test_support;
