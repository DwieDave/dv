//! Choosing in-memory or streaming storage for a document (FR-22).

use std::process::Command;

use thiserror::Error;

use crate::cli::Mode;
use crate::format::Format;

/// Where the document lives while browsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    Memory,
    Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ModeError {
    #[error("YAML files this large cannot be streamed; convert to JSON first (e.g. yq -o=json)")]
    YamlTooLarge,
}

/// Largest document kept in memory under `auto`.
pub const MAX_AUTO_MEMORY: u64 = 256_000_000;

/// The auto threshold for a machine with `ram` bytes.
#[must_use]
pub fn threshold(ram: u64) -> u64 {
    MAX_AUTO_MEMORY.min(ram / 4)
}

/// Decides the storage for a document of `len` bytes (`None` when unknown, e.g. stdin).
///
/// # Errors
/// `YamlTooLarge` when YAML would have to be streamed.
pub fn choose(
    mode: Mode,
    len: Option<u64>,
    ram: u64,
    format: Format,
) -> Result<Storage, ModeError> {
    let too_big = len.is_some_and(|len| len > threshold(ram));
    let storage = match mode {
        Mode::Stream => Storage::Stream,
        Mode::Auto if too_big => Storage::Stream,
        Mode::Memory | Mode::Auto => Storage::Memory,
    };
    match (storage, format) {
        (Storage::Stream, Format::Yaml) => Err(ModeError::YamlTooLarge),
        _ => Ok(storage),
    }
}

/// Total physical memory, from `sysctl hw.memsize` (16 GiB when unavailable).
#[must_use]
pub fn system_ram() -> u64 {
    const FALLBACK: u64 = 16 << 30;
    let output = Command::new("sysctl").args(["-n", "hw.memsize"]).output();
    output
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(FALLBACK)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const GB: u64 = 1_000_000_000;

    proptest! {
        #[test]
        fn threshold_is_the_smaller_of_the_cap_and_a_quarter_of_ram(ram in 0u64..(1 << 40)) {
            prop_assert_eq!(threshold(ram), MAX_AUTO_MEMORY.min(ram / 4));
        }
    }

    #[test]
    fn decision_table() {
        let ram = 16 * GB;
        let table = [
            (
                Mode::Memory,
                Some(10 * GB),
                Format::Json,
                Ok(Storage::Memory),
            ),
            (Mode::Stream, Some(10), Format::Json, Ok(Storage::Stream)),
            (
                Mode::Stream,
                Some(10),
                Format::Yaml,
                Err(ModeError::YamlTooLarge),
            ),
            (Mode::Auto, None, Format::Json, Ok(Storage::Memory)),
            (
                Mode::Auto,
                Some(MAX_AUTO_MEMORY),
                Format::Ndjson,
                Ok(Storage::Memory),
            ),
            (
                Mode::Auto,
                Some(MAX_AUTO_MEMORY + 1),
                Format::Ndjson,
                Ok(Storage::Stream),
            ),
            (
                Mode::Auto,
                Some(MAX_AUTO_MEMORY + 1),
                Format::Yaml,
                Err(ModeError::YamlTooLarge),
            ),
            (
                Mode::Auto,
                Some(MAX_AUTO_MEMORY + 1),
                Format::Yaml,
                Err(ModeError::YamlTooLarge),
            ),
        ];
        for (mode, len, format, expected) in table {
            assert_eq!(
                choose(mode, len, ram, format),
                expected,
                "{mode:?} {len:?} {format:?}"
            );
        }
        assert_eq!(
            choose(Mode::Auto, Some(2 * GB), 4 * GB, Format::Json),
            Ok(Storage::Stream),
            "small machines stream sooner"
        );
    }

    #[test]
    fn system_ram_is_known() {
        assert!(system_ram() > GB);
    }
}
