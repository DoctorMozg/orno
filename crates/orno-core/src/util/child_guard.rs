//! Child supervision shared by `kind: shell` nodes and the `Bash` tool.
//!
//! Dropping a `tokio` child only kills the direct process, so a
//! grandchild keeps running and keeps the inherited pipes open. The
//! guard runs the child as a process-group leader (Unix) and SIGKILLs the
//! whole group when the child exits or the guard is dropped (timeout,
//! cancellation), and aborts its reader/writer tasks.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tracing::{debug, warn};

/// Pipe drain buffer size. Fits a typical pipe buffer (64 KiB on Linux,
/// 16 KiB on macOS) so the child rarely blocks waiting for the reader.
const READ_CHUNK_BYTES: usize = 16 * 1024;

/// How long to wait for pipe EOF after the child exits. A process that
/// escaped the group (e.g. via `setsid`) can still hold the pipes open;
/// without a bound the caller would hang until it exits.
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Bytes captured from one pipe plus the total the child wrote.
#[derive(Debug)]
pub(crate) struct Capture {
    pub bytes: Vec<u8>,
    pub total: usize,
}

/// Everything [`ChildGuard::run_capped_within`] observed about a finished child.
#[derive(Debug)]
pub(crate) struct Collected {
    pub status: ExitStatus,
    pub stdout: Capture,
    pub stderr: Capture,
    /// Failure of the stdin writer, if a payload was supplied and could
    /// not be fully delivered (typically `BrokenPipe`).
    pub stdin_error: Option<std::io::Error>,
}

/// How [`ChildGuard::run_capped_within`] ended.
#[derive(Debug)]
pub(crate) enum Finished {
    Exited(Collected),
    /// The time limit hit first; the output is the prefix captured so far.
    TimedOut {
        stdout: Capture,
        stderr: Capture,
    },
}

/// Owns a spawned child and its I/O tasks; see the module docs.
pub(crate) struct ChildGuard {
    child: Child,
    stdin_payload: Option<String>,
    tasks: Vec<AbortHandle>,
    #[cfg(unix)]
    pgid: Option<rustix::process::Pid>,
}

impl ChildGuard {
    /// Spawn `cmd` with piped stdout/stderr, a piped stdin when
    /// `stdin_payload` is `Some` (null otherwise), and its own process
    /// group on Unix.
    pub(crate) fn spawn(cmd: &mut Command, stdin_payload: Option<String>) -> std::io::Result<Self> {
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin_payload.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.spawn()?;
        #[cfg(unix)]
        let pgid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw);
        Ok(Self {
            child,
            stdin_payload,
            tasks: Vec::new(),
            #[cfg(unix)]
            pgid,
        })
    }

    /// Drive the child to completion, capturing at most `cap` bytes per
    /// output stream. With a `limit`, the process group is killed once it
    /// elapses and the prefix captured so far comes back. Dropping the
    /// returned future kills the process group.
    pub(crate) async fn run_capped_within(
        mut self,
        cap: usize,
        limit: Option<Duration>,
    ) -> std::io::Result<Finished> {
        let (stop_tx, stop_rx) = watch::channel(false);

        // Written from its own task so a child that fills its stdout pipe
        // before consuming stdin cannot deadlock against us.
        let writer = self.stdin_payload.take().map(|payload| {
            let mut stdin = self.child.stdin.take().expect("stdin was piped");
            let handle = tokio::spawn(async move {
                let res = stdin.write_all(payload.as_bytes()).await;
                drop(stdin.shutdown().await);
                res
            });
            self.tasks.push(handle.abort_handle());
            handle
        });

        let stdout = self.child.stdout.take().expect("stdout was piped");
        let stderr = self.child.stderr.take().expect("stderr was piped");
        let mut stdout_task = tokio::spawn(read_capped(stdout, cap, stop_rx.clone()));
        let mut stderr_task = tokio::spawn(read_capped(stderr, cap, stop_rx));
        self.tasks.push(stdout_task.abort_handle());
        self.tasks.push(stderr_task.abort_handle());

        let status = match limit {
            Some(limit) => match tokio::time::timeout(limit, self.child.wait()).await {
                Ok(status) => Some(status?),
                Err(_elapsed) => None,
            },
            None => Some(self.child.wait().await?),
        };
        if status.is_none() {
            self.child.start_kill()?;
        }
        // The leader is gone (or killed); reap stragglers so they release the pipes.
        self.kill_group();

        let joined = tokio::time::timeout(PIPE_DRAIN_GRACE, async {
            tokio::join!(&mut stdout_task, &mut stderr_task)
        })
        .await;
        let (stdout, stderr) = if let Ok(pair) = joined {
            pair
        } else {
            warn!("child pipes still open after exit; returning captured prefix");
            // Readers hand back what they hold instead of being aborted.
            stop_tx.send_replace(true);
            (stdout_task.await, stderr_task.await)
        };
        let stdout = join_reader(stdout, "stdout")?;
        let stderr = join_reader(stderr, "stderr")?;

        let stdin_error = writer_outcome(writer).await;

        let Some(status) = status else {
            return Ok(Finished::TimedOut { stdout, stderr });
        };
        Ok(Finished::Exited(Collected {
            status,
            stdout,
            stderr,
            stdin_error,
        }))
    }

    #[cfg(unix)]
    fn kill_group(&self) {
        let Some(pgid) = self.pgid else { return };
        // ESRCH just means every member already exited.
        if let Err(err) = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL)
            && err != rustix::io::Errno::SRCH
        {
            warn!(process.pgid = pgid.as_raw_nonzero().get(), error = %err, "killing process group failed");
        } else {
            debug!(
                process.pgid = pgid.as_raw_nonzero().get(),
                "sent SIGKILL to process group"
            );
        }
    }

    #[cfg(not(unix))]
    #[expect(
        clippy::unused_self,
        reason = "no process groups off Unix; kill_on_drop covers the leader"
    )]
    fn kill_group(&self) {}
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_group();
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// How the stdin writer ended, waiting briefly for it and aborting a
/// writer stuck on a child that never reads.
async fn writer_outcome(
    writer: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
) -> Option<std::io::Error> {
    let mut handle = writer?;
    match tokio::time::timeout(PIPE_DRAIN_GRACE, &mut handle).await {
        Ok(Ok(Ok(()))) => None,
        Ok(Ok(Err(e))) => Some(e),
        Ok(Err(join_err)) => Some(std::io::Error::other(format!(
            "stdin writer task failed: {join_err}"
        ))),
        Err(_) => {
            handle.abort();
            None
        },
    }
}

fn join_reader(
    joined: Result<std::io::Result<Capture>, tokio::task::JoinError>,
    stream: &str,
) -> std::io::Result<Capture> {
    joined.map_err(|e| std::io::Error::other(format!("{stream} reader task failed: {e}")))?
}

/// Stream `pipe` into a buffer capped at `cap` bytes, counting the rest.
/// Overflow is drained so the child never blocks on a full pipe. Stops
/// early, returning what it has, when `stop` fires.
pub(crate) async fn read_capped<R>(
    mut pipe: R,
    cap: usize,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<Capture>
where
    R: AsyncReadExt + Unpin + Send,
{
    let mut bytes: Vec<u8> = Vec::with_capacity(cap.min(READ_CHUNK_BYTES));
    let mut total: usize = 0;
    let mut scratch = vec![0u8; READ_CHUNK_BYTES];

    loop {
        let n = tokio::select! {
            biased;
            read = pipe.read(&mut scratch) => read?,
            _ = stop.changed() => break,
        };
        if n == 0 {
            break;
        }
        total += n;
        let take = cap.saturating_sub(bytes.len()).min(n);
        bytes.extend_from_slice(&scratch[..take]);
    }

    if bytes.len() == cap {
        bytes.truncate(utf8_safe_len(&bytes));
    }
    Ok(Capture { bytes, total })
}

/// Length of `buf` with a trailing, cut-off multi-byte sequence removed.
/// Looks at most four bytes back, so a stream of stray continuation bytes
/// is left alone rather than eaten.
fn utf8_safe_len(buf: &[u8]) -> usize {
    let n = buf.len();
    for back in 1..=n.min(4) {
        let byte = buf[n - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let needed = match byte {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if needed > back { n - back } else { n };
    }
    n
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::time::Duration;

    /// Wait for a file holding a pid to appear and parse it.
    pub(crate) async fn read_pid_file(path: &Path) -> u32 {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("pid file {} never appeared", path.display());
    }

    /// True once `pid` is gone or only a zombie awaiting reaping (a
    /// container's pid 1 may never reap it).
    pub(crate) async fn wait_until_gone(pid: u32) -> bool {
        for _ in 0..60 {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .expect("ps available");
            let stat = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() || stat.trim().is_empty() || stat.trim().starts_with('Z') {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    async fn capture(input: &[u8], cap: usize) -> Capture {
        let (_tx, rx) = watch::channel(false);
        read_capped(input, cap, rx).await.expect("read succeeds")
    }

    // "a" + "é" (2 bytes) + "€" (3) + "😀" (4) + "b" = 11 bytes.
    #[rstest]
    #[case(1, "a")]
    #[case(2, "a")]
    #[case(3, "aé")]
    #[case(4, "aé")]
    #[case(5, "aé")]
    #[case(6, "aé€")]
    #[case(7, "aé€")]
    #[case(8, "aé€")]
    #[case(9, "aé€")]
    #[case(10, "aé€😀")]
    #[case(11, "aé€😀b")]
    #[tokio::test]
    async fn truncation_lands_on_a_char_boundary(#[case] cap: usize, #[case] expected: &str) {
        let text = "aé€😀b";
        let got = capture(text.as_bytes(), cap).await;
        assert_eq!(got.total, text.len(), "total counts every byte written");
        assert_eq!(
            std::str::from_utf8(&got.bytes).expect("valid UTF-8"),
            expected
        );
    }

    #[tokio::test]
    async fn stray_continuation_bytes_are_not_eaten() {
        let got = capture(&[0x80; 16], 8).await;
        assert_eq!(got.bytes.len(), 8);
    }
}
