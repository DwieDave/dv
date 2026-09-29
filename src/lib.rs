//! dv: a fast terminal viewer for large JSON, NDJSON and YAML files.
#![forbid(unsafe_code)]

pub mod error;
pub mod index;
pub mod json;
pub mod position;
pub mod source;

#[cfg(test)]
mod test_support;
