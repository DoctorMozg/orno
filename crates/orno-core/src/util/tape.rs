//! Shared NDJSON tape I/O for the LLM and tool record/replay layers.
//!
//! Recording appends one JSON line per entry to a [`TapeFile`]; replay
//! loads the lines with [`read_tape`] and serves them from a
//! [`TapeIndex`], which hands out entries per `(key, seq)` so repeated
//! identical requests come back in the order they were made.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Append-only tape file shared by every recorder writing to it.
///
/// Created with `O_EXCL` and mode 0600: a pre-planted symlink cannot
/// redirect the writes, a stale tape is never silently reused, and the
/// captured payloads are not readable by other local users. A poisoned
/// lock is recovered everywhere; the worst case is one torn line, which
/// the replay loader rejects with its line number.
pub struct TapeFile {
    writer: Mutex<BufWriter<File>>,
    path: PathBuf,
}

// `BufWriter<File>` has no `Debug`, so derive cannot cover this.
impl std::fmt::Debug for TapeFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapeFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl TapeFile {
    /// Create a new tape at `path`, failing if the path already exists.
    pub fn create(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
            path,
        })
    }

    /// Wrap an already-open file with no buffering, so a failing write
    /// device surfaces on the first `append`.
    #[cfg(test)]
    pub(crate) fn from_unbuffered(file: File, path: PathBuf) -> Self {
        Self {
            writer: Mutex::new(BufWriter::with_capacity(0, file)),
            path,
        }
    }

    /// Path the tape is being written to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serialize `entry` and append it as one line. Serialization
    /// happens before the lock is taken, so a failing entry never
    /// leaves a partial line behind.
    pub fn append<T: Serialize>(&self, entry: &T) -> std::io::Result<()> {
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::other(format!("tape serialize failed: {e}")))?;
        writeln!(self.lock(), "{line}")
    }

    /// Flush buffered bytes and fsync. `BufWriter::flush` only reaches
    /// the page cache, which a power cut can lose.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut writer = self.lock();
        writer.flush()?;
        writer.get_mut().sync_all()
    }

    fn lock(&self) -> MutexGuard<'_, BufWriter<File>> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Load every entry of a tape file in file order. Blank lines are
/// skipped; a line that fails to parse yields an error naming the file
/// and the 1-indexed line.
pub fn read_tape<T: DeserializeOwned>(path: impl AsRef<Path>) -> std::io::Result<Vec<T>> {
    let path = path.as_ref();
    let file = File::open(path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    let mut entries = Vec::new();
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry = serde_json::from_str(&line)
            .map_err(|e| std::io::Error::other(format!("{}:{}: {e}", path.display(), idx + 1)))?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Recording side: numbers the calls made with each key so replay can
/// tell identical requests apart even when completion order differs
/// from call order.
#[derive(Debug, Default)]
pub struct SeqCounter {
    next: Mutex<HashMap<String, u32>>,
}

impl SeqCounter {
    /// Take the next sequence number for `key`, starting at 0.
    pub fn next(&self, key: &str) -> u32 {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = next.entry(key.to_string()).or_insert(0);
        let seq = *slot;
        *slot += 1;
        seq
    }
}

/// Two tape entries claimed the same `(key, seq)` slot.
#[derive(Debug, thiserror::Error)]
#[error("duplicate tape entry for key `{key}` seq {seq}")]
pub struct DuplicateTapeEntry {
    /// Key shared by both entries.
    pub key: String,
    /// Sequence number shared by both entries.
    pub seq: u32,
}

/// Replay side: entries indexed by `(key, seq)`. The n-th call with a
/// key takes the entry recorded with seq n, and every entry is served
/// at most once.
#[derive(Debug)]
pub struct TapeIndex<V> {
    state: Mutex<IndexState<V>>,
}

#[derive(Debug)]
struct IndexState<V> {
    entries: HashMap<(String, u32), V>,
    next: HashMap<String, u32>,
}

impl<V> TapeIndex<V> {
    /// Index `(key, seq, value)` triples; a repeated `(key, seq)` is an error.
    pub fn new(
        entries: impl IntoIterator<Item = (String, u32, V)>,
    ) -> Result<Self, DuplicateTapeEntry> {
        let mut map = HashMap::new();
        for (key, seq, value) in entries {
            if map.contains_key(&(key.clone(), seq)) {
                return Err(DuplicateTapeEntry { key, seq });
            }
            map.insert((key, seq), value);
        }
        Ok(Self {
            state: Mutex::new(IndexState {
                entries: map,
                next: HashMap::new(),
            }),
        })
    }

    /// Take the entry for the next call made with `key`, or `None` when
    /// the tape holds no entry for that call.
    pub fn take(&self, key: &str) -> Option<V> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = state.next.entry(key.to_string()).or_insert(0);
        let seq = *slot;
        *slot += 1;
        state.entries.remove(&(key.to_string(), seq))
    }

    /// Number of entries not yet served.
    pub fn remaining(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .len()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Entry {
        key: String,
        value: u32,
    }

    fn entry(key: &str, value: u32) -> Entry {
        Entry {
            key: key.into(),
            value,
        }
    }

    #[test]
    fn round_trip_preserves_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tape.ndjson");
        let tape = TapeFile::create(&path).unwrap();
        let entries = vec![entry("a", 1), entry("b", 2), entry("c", 3)];
        for e in &entries {
            tape.append(e).unwrap();
        }
        tape.flush().unwrap();

        assert_eq!(read_tape::<Entry>(&path).unwrap(), entries);
    }

    #[test]
    fn create_fails_when_file_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tape.ndjson");
        std::fs::write(&path, b"").unwrap();

        let err = TapeFile::create(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[cfg(unix)]
    #[test]
    fn create_sets_mode_0o600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tape.ndjson");
        TapeFile::create(&path).unwrap().flush().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "tape file must not be world-readable");
    }

    #[test]
    fn read_skips_blank_lines_and_names_corrupt_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tape.ndjson");
        let good = serde_json::to_string(&entry("ok", 0)).unwrap();
        std::fs::write(&path, format!("{good}\n\n{good}\nnot-json\n")).unwrap();

        let err = read_tape::<Entry>(&path).unwrap_err().to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(":4:"), "{err}");
    }

    #[test]
    fn append_surfaces_serialize_failure_without_writing() {
        struct FailSer;
        impl Serialize for FailSer {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("deliberate failure"))
            }
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tape.ndjson");
        let tape = TapeFile::create(&path).unwrap();

        let err = tape.append(&FailSer).unwrap_err();
        assert!(err.to_string().contains("tape serialize failed"), "{err}");
        tape.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn index_serves_entries_by_call_order_not_file_order() {
        // Recorded in completion order: the second call finished first.
        let index = TapeIndex::new([
            ("k".to_string(), 1, "second"),
            ("k".to_string(), 0, "first"),
        ])
        .unwrap();

        assert_eq!(index.take("k"), Some("first"));
        assert_eq!(index.take("k"), Some("second"));
        assert_eq!(index.take("k"), None);
        assert_eq!(index.take("other"), None);
    }

    #[test]
    fn index_rejects_duplicate_slot() {
        let err = TapeIndex::new([("k".to_string(), 0, 1), ("k".to_string(), 0, 2)]).unwrap_err();
        assert_eq!((err.key.as_str(), err.seq), ("k", 0));
    }

    #[test]
    fn seq_counter_numbers_each_key_independently() {
        let counter = SeqCounter::default();
        assert_eq!(counter.next("a"), 0);
        assert_eq!(counter.next("a"), 1);
        assert_eq!(counter.next("b"), 0);
    }
}
