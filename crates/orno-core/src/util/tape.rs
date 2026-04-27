//! Generic NDJSON tape I/O shared by the LLM and tool record/replay layers.
//!
//! Both layers persist their entries as newline-delimited JSON: one serialized
//! struct per line, created with `O_EXCL` so stale tapes cannot be silently
//! overwritten, and fsynced on flush so a power cut cannot corrupt the bounded
//! non-determinism guarantee.
//!
//! `TapeWriter<T>` and `TapeReader<T>` centralize that file-I/O contract so
//! neither the LLM nor the tool layer needs its own copy of the open/flush/
//! sync/parse boilerplate.

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
