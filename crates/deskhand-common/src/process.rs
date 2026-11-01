//! Running user-configured commands (scripts and command sensors).
//!
//! Commands come only from the config file, never from MQTT. They run with
//! stdin closed, in their own process group so a timeout or shutdown kills
//! everything they started, and with captured output capped in size.

use std::fmt;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

/// Captured output beyond this many bytes per stream is discarded.
pub const OUTPUT_LIMIT: usize = 64 * 1024;

/// `command = ["argv", "..."]`, or `command = "a | b"` together with `shell = true`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum CommandLine {
    Argv(Vec<String>),
    Shell(String),
}

/// Why a configured command is unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandLineError {
    #[error("command is empty")]
    Empty,
    #[error("command is a string; use an argv array or set shell = true")]
    StringWithoutShell,
    #[error("shell = true needs command to be a single string")]
    ArgvWithShell,
}

impl CommandLine {
    /// The argv to execute. With `shell`, the string runs under `sh -c`.
    pub fn argv(&self, shell: bool) -> Result<Vec<String>, CommandLineError> {
        let argv = match (self, shell) {
            (Self::Argv(argv), false) => argv.clone(),
            (Self::Shell(s), true) if s.trim().is_empty() => return Err(CommandLineError::Empty),
            (Self::Shell(s), true) => vec!["sh".into(), "-c".into(), s.clone()],
            (Self::Shell(_), false) => return Err(CommandLineError::StringWithoutShell),
            (Self::Argv(_), true) => return Err(CommandLineError::ArgvWithShell),
        };
        if argv.first().is_none_or(|p| p.is_empty()) {
            return Err(CommandLineError::Empty);
        }
        Ok(argv)
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Exited with this code.
    Exited(i32),
    /// Killed by a signal it did not expect (not by us).
    Signalled,
    TimedOut,
    /// Deskhand is stopping; the command was killed.
    Cancelled,
}

impl Outcome {
    pub fn success(self) -> bool {
        self == Self::Exited(0)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited(code) => write!(f, "exited with {code}"),
            Self::Signalled => f.write_str("killed by a signal"),
            Self::TimedOut => f.write_str("timed out"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// Result of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub outcome: Outcome,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

/// Runs `argv` to completion, killing its process group on timeout or cancel.
/// Fails only if the program cannot be started.
pub async fn run(
    argv: &[String],
    timeout: Duration,
    cancel: &CancellationToken,
) -> std::io::Result<Output> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let started = Instant::now();
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let wait = async {
        let (status, stdout, stderr) =
            tokio::join!(child.wait(), read_capped(stdout), read_capped(stderr));
        (status, stdout, stderr)
    };

    let (outcome, stdout, stderr) = tokio::select! {
        (status, stdout, stderr) = wait => (exit_outcome(status?), stdout, stderr),
        () = tokio::time::sleep(timeout) => {
            kill_group(pid);
            (Outcome::TimedOut, String::new(), String::new())
        }
        () = cancel.cancelled() => {
            kill_group(pid);
            (Outcome::Cancelled, String::new(), String::new())
        }
    };
    Ok(Output {
        outcome,
        stdout,
        stderr,
        duration: started.elapsed(),
    })
}

fn exit_outcome(status: ExitStatus) -> Outcome {
    status.code().map_or(Outcome::Signalled, Outcome::Exited)
}

/// Reads a stream to the end, keeping the first [`OUTPUT_LIMIT`] bytes.
async fn read_capped(stream: Option<impl AsyncRead + Unpin>) -> String {
    let Some(mut stream) = stream else {
        return String::new();
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = OUTPUT_LIMIT.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Kills the whole process group the command leads (it may have children).
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid
        .and_then(|p| i32::try_from(p).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    #[cfg(not(unix))]
    let _ = pid; // kill_on_drop kills the direct child.
}

/// The last `max` characters of `text`, for logs and attributes.
pub fn tail(text: &str, max: usize) -> &str {
    let text = text.trim_end();
    let skip = text.chars().count().saturating_sub(max);
    text.char_indices()
        .nth(skip)
        .map_or("", |(i, _)| &text[i..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_line_forms() {
        let a = CommandLine::Argv(argv(&["echo", "hi"]));
        assert_eq!(a.argv(false).unwrap(), argv(&["echo", "hi"]));
        assert_eq!(a.argv(true), Err(CommandLineError::ArgvWithShell));
        let s = CommandLine::Shell("echo hi | tr a-z A-Z".into());
        assert_eq!(
            s.argv(true).unwrap(),
            argv(&["sh", "-c", "echo hi | tr a-z A-Z"])
        );
        assert_eq!(s.argv(false), Err(CommandLineError::StringWithoutShell));
        assert_eq!(
            CommandLine::Argv(vec![]).argv(false),
            Err(CommandLineError::Empty)
        );
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("hello\n", 3), "llo");
        assert_eq!(tail("hi", 10), "hi");
        assert_eq!(tail("héllo", 4), "éllo");
        assert_eq!(tail("abc", 0), "");
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let out = run(
            &argv(&["sh", "-c", "echo out; echo err >&2; exit 3"]),
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.outcome, Outcome::Exited(3));
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
    }

    #[tokio::test]
    async fn timeout_kills_the_process_group() {
        let dir = std::env::temp_dir().join(format!("deskhand-pg-{}", std::process::id()));
        let marker = dir.with_extension("marker");
        let _ = std::fs::remove_file(&marker);
        // The grandchild would create the marker after 1 s unless the group is killed.
        let script = format!("(sleep 1; touch {}) & wait", marker.display());
        let out = run(
            &argv(&["sh", "-c", &script]),
            Duration::from_millis(200),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.outcome, Outcome::TimedOut);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "grandchild survived the timeout");
    }

    #[tokio::test]
    async fn output_is_capped() {
        let out = run(
            &argv(&["sh", "-c", "head -c 200000 /dev/zero | tr '\\0' x"]),
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.outcome.success());
        assert_eq!(out.stdout.len(), OUTPUT_LIMIT);
    }

    #[tokio::test]
    async fn missing_program_is_an_error() {
        let err = run(
            &argv(&["/nonexistent/deskhand-test"]),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await;
        assert!(err.is_err());
    }
}
