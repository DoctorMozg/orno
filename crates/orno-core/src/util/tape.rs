//! Generic NDJSON tape I/O for the LLM and tool record/replay layers.
//!
//! Both layers persist their entries as newline-delimited JSON: one serialized
//! struct per line, created with `O_EXCL` so stale tapes cannot be silently
//! overwritten, and fsynced on flush so a power cut cannot corrupt the bounded
//! non-determinism guarantee.
//!
//! `TapeWriter<T>` and `TapeReader<T>` centralize that file-I/O contract.
//! Migration of `llm/recording.rs`, `llm/replay.rs`, `tool/record.rs`, and
//! `tool/replay.rs` to use these types is deferred — the LLM layer requires a
//! redactor step between serialize and write, and the tool layer uses an
//! `Arc<Mutex<BufWriter>>` for shared-tape writers across handlers; both need
//! design work before the inlined boilerplate can be replaced.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Append-only NDJSON tape writer. Each call to [`TapeWriter::write`]
/// serializes one entry and appends it to the underlying `BufWriter`.
/// Call [`TapeWriter::flush`] after the run to push buffered bytes to
/// kernel and fsync to stable storage.
///
/// Created with `O_EXCL` — the path must not already exist. This
/// prevents a pre-created symlink at the tape path from redirecting
/// writes to an attacker-controlled location, and forces the caller
/// to make an explicit decision when a stale tape sits at the path.
pub struct TapeWriter<T> {
    writer: BufWriter<File>,
    path: PathBuf,
    _phantom: PhantomData<T>,
}

// `BufWriter<File>` does not implement `Debug`, so derive cannot cover this.
impl<T> std::fmt::Debug for TapeWriter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapeWriter")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<T: Serialize> TapeWriter<T> {
    /// Create a new tape at `path`, failing if the file already exists.
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, std::io::Error> {
        let path = path.into();
        let mut opts = OpenOptions::new();
        // O_EXCL: prevent symlink races and stale-tape silent reuse.
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Tape files capture full request/response payloads —
            // they must not be world-readable on shared hosts.
            opts.mode(0o600);
        }
        let file = opts.open(&path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            path,
            _phantom: PhantomData,
        })
    }

    /// Path the tape is being written to. Primarily useful for tests
    /// that need to round-trip through a [`TapeReader`].
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serialize `entry` and append it as a single NDJSON line.
    pub fn write(&mut self, entry: &T) -> Result<(), std::io::Error> {
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::other(format!("tape serialize failed: {e}")))?;
        writeln!(self.writer, "{line}")
    }

    /// Flush buffered bytes to kernel and fsync to stable storage.
    ///
    /// A plain `BufWriter::flush` pushes bytes to the OS page cache but
    /// does not guarantee durability across a power cut. `sync_all` closes
    /// that gap so replay always sees a fully-written tape.
    pub fn flush(&mut self) -> Result<(), std::io::Error> {
        self.writer.flush()?;
        self.writer.get_mut().sync_all()
    }
}

/// NDJSON tape reader. Loads all entries up-front from a tape file,
/// parsing each line into `T`. A corrupt line surfaces as a structured
/// error that names the file path and the 1-indexed line number so
/// operators can identify exactly which entry is broken.
///
/// Entries are returned as a flat `Vec<T>` — the caller is responsible
/// for any keying, deduplication, or FIFO-per-key indexing it needs.
pub struct TapeReader<T> {
    _phantom: PhantomData<T>,
}

impl<T: DeserializeOwned> TapeReader<T> {
    /// Load all entries from `path`. Returns the parsed entries in file
    /// order. Blank lines are skipped; any non-blank line that fails to
    /// parse produces an `Err` naming the file and the 1-indexed line
    /// number.
    pub fn load(path: impl AsRef<Path>) -> Result<Vec<T>, std::io::Error> {
        let path = path.as_ref();
        let path_disp = path.display().to_string();
        let file = File::open(path)?;
        let mut entries = Vec::new();
        for (lineno, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: T = serde_json::from_str(&line)
                .map_err(|e| std::io::Error::other(format!("{path_disp}:{}: {e}", lineno + 1)))?;
            entries.push(entry);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Entry {
        key: String,
        value: u32,
    }

    fn tmp_path(dir: &TempDir) -> PathBuf {
        dir.path().join("tape.ndjson")
    }

    #[test]
    fn round_trip_preserves_order() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir);

        let entries = vec![
            Entry {
                key: "a".into(),
                value: 1,
            },
            Entry {
                key: "b".into(),
                value: 2,
            },
            Entry {
                key: "c".into(),
                value: 3,
            },
        ];

        let mut writer = TapeWriter::create(&path).unwrap();
        for e in &entries {
            writer.write(e).unwrap();
        }
        writer.flush().unwrap();

        let loaded: Vec<Entry> = TapeReader::load(&path).unwrap();
        assert_eq!(loaded, entries);
    }

    #[test]
    fn create_fails_when_file_exists() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir);
        // Pre-create the file so the second open must fail.
        std::fs::write(&path, b"").unwrap();

        let err = TapeWriter::<Entry>::create(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[cfg(unix)]
    #[test]
    fn create_sets_mode_0o600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir);
        let mut w = TapeWriter::<Entry>::create(&path).unwrap();
        w.flush().unwrap();
        let perm = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            perm.mode() & 0o777,
            0o600,
            "tape file must not be world-readable"
        );
    }

    #[test]
    fn load_skips_blank_lines() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir);
        let line = serde_json::to_string(&Entry {
            key: "x".into(),
            value: 7,
        })
        .unwrap();
        std::fs::write(&path, format!("{line}\n\n{line}\n")).unwrap();

        let loaded: Vec<Entry> = TapeReader::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn load_reports_path_and_lineno_on_corrupt_entry() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir);
        let good = serde_json::to_string(&Entry {
            key: "ok".into(),
            value: 0,
        })
        .unwrap();
        std::fs::write(&path, format!("{good}\nnot-json\n")).unwrap();

        let err = TapeReader::<Entry>::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(path.display().to_string().as_str()),
            "error should name the tape file: {msg}"
        );
        assert!(
            msg.contains(":2:"),
            "error should include 1-indexed line number: {msg}"
        );
    }

    #[test]
    fn flush_on_empty_writer_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir);
        let mut writer = TapeWriter::<Entry>::create(&path).unwrap();
        writer.flush().unwrap();
        let loaded: Vec<Entry> = TapeReader::load(&path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn write_surfaces_serialize_failure_with_named_error() {
        // Regression for the `serde_json::to_string -> map_err` branch in
        // `write`. A type that always fails to serialize must surface as a
        // `std::io::Error` whose payload mentions "tape serialize failed", so
        // operators see the failure cause rather than a bare `Other` kind.
        struct FailSer;
        impl Serialize for FailSer {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("deliberate failure"))
            }
        }

        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir);
        let mut writer = TapeWriter::<FailSer>::create(&path).unwrap();
        let err = writer
            .write(&FailSer)
            .expect_err("serialize failure must propagate as io::Error");
        let msg = err.to_string();
        assert!(
            msg.contains("tape serialize failed"),
            "error must name the wrapping context, got {msg:?}",
        );
    }
}
