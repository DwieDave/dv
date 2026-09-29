//! Remembered cursor positions per file, under `$XDG_STATE_HOME/dv` (HI-3).

use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Files remembered at most.
const LIMIT: usize = 500;

/// A file as it was: a position only applies while the file is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileKey {
    /// The canonical path.
    pub path: String,
    pub size: u64,
    /// Modification time in nanoseconds since the epoch.
    pub modified: u128,
}

impl FileKey {
    /// The key of the file at `path`, when it can be read.
    #[must_use]
    pub fn of(path: &Path) -> Option<Self> {
        let canonical = path.canonicalize().ok()?;
        let meta = canonical.metadata().ok()?;
        let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            path: canonical.to_string_lossy().into_owned(),
            size: meta.len(),
            modified: modified.as_nanos(),
        })
    }
}

/// `$XDG_STATE_HOME/dv/positions.tsv`, else `~/.local/state/dv/positions.tsv`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    let home = || std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state"));
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(home)?;
    Some(dir.join("dv").join("positions.tsv"))
}

/// Cursor row paths by file, least recently used first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Positions {
    entries: Vec<(FileKey, Vec<u64>)>,
}

impl Positions {
    /// The positions in `file`; missing or unreadable lines are skipped.
    #[must_use]
    pub fn load(file: &Path) -> Self {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        Self {
            entries: text.lines().filter_map(parse_line).collect(),
        }
    }

    #[must_use]
    pub fn get(&self, key: &FileKey) -> Option<&Vec<u64>> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, rows)| rows)
    }

    /// Remembers `rows` for the file at `key.path`, as the most recently used.
    pub fn put(&mut self, key: FileKey, rows: Vec<u64>) {
        self.entries.retain(|(k, _)| k.path != key.path);
        self.entries.push((key, rows));
    }

    /// Writes the newest [`LIMIT`] entries next to `file`, then renames it into place.
    ///
    /// # Errors
    /// When the directory or file cannot be written.
    pub fn save(&self, file: &Path) -> io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let keep = self.entries.len().saturating_sub(LIMIT);
        let text: String = self.entries[keep..]
            .iter()
            .map(|(k, rows)| format_line(k, rows))
            .collect();
        let tmp = file.with_extension("tsv.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, file)
    }
}

/// `path⇥size⇥modified⇥rows`, with `\`, tab and newline escaped in the path.
fn format_line(key: &FileKey, rows: &[u64]) -> String {
    let path = key
        .path
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n");
    let rows: Vec<String> = rows.iter().map(u64::to_string).collect();
    format!(
        "{path}\t{}\t{}\t{}\n",
        key.size,
        key.modified,
        rows.join(",")
    )
}

fn parse_line(line: &str) -> Option<(FileKey, Vec<u64>)> {
    let mut fields = line.split('\t');
    let path = unescape(fields.next()?)?;
    let size = fields.next()?.parse().ok()?;
    let modified = fields.next()?.parse().ok()?;
    let rows = fields.next()?;
    let rows = if rows.is_empty() {
        Vec::new()
    } else {
        rows.split(',')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?
    };
    fields.next().is_none().then_some((
        FileKey {
            path,
            size,
            modified,
        },
        rows,
    ))
}

fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        out.push(match c {
            '\\' => match chars.next()? {
                't' => '\t',
                'n' => '\n',
                '\\' => '\\',
                _ => return None,
            },
            c => c,
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn key(path: &str, size: u64) -> FileKey {
        FileKey {
            path: path.to_owned(),
            size,
            modified: 7,
        }
    }

    proptest! {
        #[test]
        fn positions_survive_a_save_and_load(entries in prop::collection::vec((any::<String>(), any::<u64>(), prop::collection::vec(any::<u64>(), 0..6)), 0..20)) {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("dv").join("positions.tsv");
            let mut positions = Positions::default();
            for (path, size, rows) in &entries {
                positions.put(key(path, *size), rows.clone());
            }
            positions.save(&file).unwrap();
            let loaded = Positions::load(&file);
            for (path, size, _) in &entries {
                prop_assert_eq!(loaded.get(&key(path, *size)), positions.get(&key(path, *size)));
            }
        }
    }

    #[test]
    fn only_the_same_unchanged_file_matches() {
        let mut positions = Positions::default();
        positions.put(key("/a.json", 10), vec![1, 2]);
        assert_eq!(positions.get(&key("/a.json", 10)), Some(&vec![1, 2]));
        assert_eq!(positions.get(&key("/a.json", 11)), None, "a changed size");
        let touched = FileKey {
            modified: 8,
            ..key("/a.json", 10)
        };
        assert_eq!(positions.get(&touched), None, "a changed modification time");
    }

    #[test]
    fn the_least_recently_used_files_are_dropped_after_500() {
        let mut positions = Positions::default();
        for i in 0..520 {
            positions.put(key(&format!("/f{i}"), 1), vec![i]);
        }
        positions.put(key("/f3", 1), vec![3]);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("positions.tsv");
        positions.save(&file).unwrap();
        let loaded = Positions::load(&file);
        assert_eq!(
            loaded.get(&key("/f3", 1)),
            Some(&vec![3]),
            "recently used again"
        );
        assert_eq!(loaded.get(&key("/f20", 1)), None, "among the 20 oldest");
        assert_eq!(loaded.get(&key("/f21", 1)), Some(&vec![21]));
        assert_eq!(loaded.get(&key("/f519", 1)), Some(&vec![519]));
    }

    #[test]
    fn a_missing_or_garbled_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Positions::load(&dir.path().join("none.tsv")),
            Positions::default()
        );
        let file = dir.path().join("bad.tsv");
        std::fs::write(&file, "garbage\tline\n").unwrap();
        assert_eq!(Positions::load(&file), Positions::default());
    }
}
