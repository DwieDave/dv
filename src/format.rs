//! Supported input formats.

/// The syntax a document was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Ndjson,
    Yaml,
}

impl Format {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Ndjson => "NDJSON",
            Self::Yaml => "YAML",
        }
    }
}
