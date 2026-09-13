//! Clipboard: platform command first, OSC52 as the fallback.
//!
//! A bare OSC52 write is not a success signal: terminals that don't implement
//! it (or a tmux without `set-clipboard on`) silently drop the payload, so the
//! app would show "Copied!" while the system clipboard still holds the old
//! content. Whenever a local helper exists we therefore write the text through
//! it and only report success on its exit status; OSC52 remains the fallback
//! (it is the only channel that works over SSH, and the only one that works
//! when no helper is installed).
//!
//! Remote sessions skip the helpers entirely: `pbcopy` / `clip` / `xclip` on
//! the *remote* host would fill that machine's clipboard, which is never what
//! the user wants to paste into their local terminal.

use std::io::Write;

use anyhow::{Context, Result};
use crossterm::clipboard::{ClipboardSelection, ClipboardType, CopyToClipboard};
use crossterm::execute;

/// Environment variables that mark the session as remote (SSH / MOSH).
const REMOTE_SESSION_MARKERS: [&str; 3] = ["SSH_CONNECTION", "SSH_CLIENT", "MOSH_CONNECTION"];

/// Largest OSC52 payload we are willing to write (base64-encoded bytes).
///
/// Bigger payloads are rejected by terminals (or silently truncated) and can
/// wedge the input buffer while the sequence is being parsed; the platform
/// command path has no such limit, so a local copy of a huge selection still
/// works.
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// Which channel actually delivered the text to the system clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPath {
    /// A local helper program (`pbcopy` / `clip` / `wl-copy` / …).
    PlatformCommand(&'static str),
    /// OSC52 escape sequence written to the terminal.
    Osc52,
}

/// One platform clipboard helper: the program plus its fixed arguments
/// (the text itself goes to stdin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommandSpec {
    program: &'static str,
    args: &'static [&'static str],
}

// The three tables below are declared for every platform (not just the host)
// so the candidate ordering is unit-testable wherever the tests run — hence
// the `dead_code` allowance for the tables of foreign platforms.
#[allow(dead_code)]
const MACOS_COMMANDS: &[CommandSpec] = &[CommandSpec {
    program: "pbcopy",
    args: &[],
}];

#[allow(dead_code)]
const WINDOWS_COMMANDS: &[CommandSpec] = &[CommandSpec {
    program: "clip",
    args: &[],
}];

/// Wayland first, then X11 (`xclip`, then `xsel`).
#[allow(dead_code)]
const LINUX_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        program: "wl-copy",
        args: &[],
    },
    CommandSpec {
        program: "xclip",
        args: &["-selection", "clipboard"],
    },
    CommandSpec {
        program: "xsel",
        args: &["--clipboard", "--input"],
    },
];

/// Helpers to try on this platform, most preferred first.
fn platform_commands() -> &'static [CommandSpec] {
    #[cfg(target_os = "macos")]
    {
        MACOS_COMMANDS
    }
    #[cfg(target_os = "windows")]
    {
        WINDOWS_COMMANDS
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        LINUX_COMMANDS
    }
}

/// Runs one clipboard helper with `text` on stdin.
///
/// Injected so the candidate order and the remote-session short-circuit are
/// unit-testable without touching the real clipboard.
trait CommandRunner {
    /// `Ok(())` means the helper was spawned **and** exited successfully.
    fn run(&self, spec: &CommandSpec, text: &str) -> std::io::Result<()>;
}

/// Production runner: `std::process::Command` with piped stdio.
struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, spec: &CommandSpec, text: &str) -> std::io::Result<()> {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        // Stdio is piped (never inherited): the TUI owns the terminal in raw
        // mode / alternate screen, and the crate denies direct printing.
        let mut child = Command::new(spec.program)
            .args(spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }
        // Drop the write end so the helper sees EOF even if it does not count
        // bytes, then reap it.
        child.stdin.take();
        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "{} exited with {}",
                spec.program, output.status
            )))
        }
    }
}

/// Whether the process environment marks this as a remote session.
fn is_remote_session() -> bool {
    is_remote_session_with(|key| std::env::var(key).ok())
}

/// Injected lookup for testability — the real one reads the environment.
fn is_remote_session_with(env: impl Fn(&str) -> Option<String>) -> bool {
    REMOTE_SESSION_MARKERS
        .iter()
        .any(|marker| env(marker).is_some_and(|value| !value.is_empty()))
}

/// Channel decided by the (synchronous, process-spawning) probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Platform(&'static str),
    Osc52,
}

/// Probe the helpers (unless remote) and decide the channel.
///
/// Synchronous on purpose: it is the part that spawns and waits for a process,
/// so production runs it on the blocking pool. The decision itself is what the
/// tests pin down — order of candidates, fall-through, remote short-circuit.
fn choose_channel(
    text: &str,
    remote: bool,
    commands: &[CommandSpec],
    runner: &impl CommandRunner,
) -> Probe {
    if remote {
        return Probe::Osc52;
    }
    for spec in commands {
        match runner.run(spec, text) {
            Ok(()) => return Probe::Platform(spec.program),
            Err(e) => tracing::debug!("clipboard helper {} unusable: {e}", spec.program),
        }
    }
    Probe::Osc52
}

/// Decide the channel by probing the platform helpers on the blocking pool.
///
/// Spawning a helper and waiting for it must never stall the event loop or a
/// frame, hence `spawn_blocking`; a panic in there degrades to the OSC52
/// fallback instead of poisoning the copy.
async fn probe_channel(text: &str) -> Probe {
    let owned = text.to_owned();
    let remote = is_remote_session();
    tokio::task::spawn_blocking(move || {
        choose_channel(&owned, remote, platform_commands(), &ProcessCommandRunner)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("clipboard helper task failed: {e}");
        Probe::Osc52
    })
}

/// Complete the copy for a decided probe: the platform channel already
/// delivered the text, OSC52 is written here.
///
/// Split out so both outcomes are unit-testable without spawning anything.
fn finish_copy(writer: &mut impl Write, text: &str, probe: Probe) -> Result<ClipboardPath> {
    match probe {
        Probe::Platform(program) => Ok(ClipboardPath::PlatformCommand(program)),
        Probe::Osc52 => {
            copy_to_clipboard(writer, text)?;
            Ok(ClipboardPath::Osc52)
        }
    }
}

/// Copy `text` to the system clipboard, platform command first.
///
/// OSC52 stays on this task: it writes to the terminal backend (`writer`),
/// which is not `Send`. Returns the channel that delivered the text; an error
/// means neither channel worked (the caller reports `Copy failed: …`).
pub(crate) async fn copy_best_effort(writer: &mut impl Write, text: &str) -> Result<ClipboardPath> {
    let probe = probe_channel(text).await;
    finish_copy(writer, text, probe)
}

/// Write `text` to the system clipboard via OSC52.
///
/// Requires a terminal that supports the OSC 52 escape sequence.
/// Works transparently over SSH and tmux (with `set-clipboard on`).
pub fn copy_to_clipboard(writer: &mut impl Write, text: &str) -> Result<()> {
    let encoded_len = text.len().div_ceil(3) * 4; // base64 expansion
    if encoded_len > MAX_OSC52_ENCODED_LENGTH {
        anyhow::bail!(
            "text too large for OSC52 ({encoded_len} encoded bytes > {MAX_OSC52_ENCODED_LENGTH}); \
             no local clipboard helper available"
        );
    }
    execute!(
        writer,
        CopyToClipboard {
            content: text.to_owned(),
            destination: ClipboardSelection(vec![ClipboardType::Clipboard]),
        }
    )
    .context("OSC52 clipboard write failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every helper invocation; fails / reports-not-installed per
    /// program so the fall-through can be asserted step by step.
    struct FakeRunner {
        fail: Vec<&'static str>,
        missing: Vec<&'static str>,
        calls: RefCell<Vec<(String, Vec<String>, String)>>,
    }

    impl FakeRunner {
        fn new(fail: &[&'static str], missing: &[&'static str]) -> Self {
            Self {
                fail: fail.to_vec(),
                missing: missing.to_vec(),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn called(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(program, _, _)| program.clone())
                .collect()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, spec: &CommandSpec, text: &str) -> std::io::Result<()> {
            self.calls.borrow_mut().push((
                spec.program.to_string(),
                spec.args.iter().map(|a| a.to_string()).collect(),
                text.to_string(),
            ));
            if self.missing.contains(&spec.program) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "not installed",
                ));
            }
            if self.fail.contains(&spec.program) {
                return Err(std::io::Error::other("exit 1"));
            }
            Ok(())
        }
    }

    #[test]
    fn copy_writes_osc52_sequence() {
        let mut buf = Vec::new();
        copy_to_clipboard(&mut buf, "hello").unwrap();
        let output = String::from_utf8(buf).unwrap();
        // OSC52 format: ESC ] 52 ; c ; <base64> ST
        assert!(output.contains("52;c;"));
        // "hello" in base64 is "aGVsbG8="
        assert!(output.contains("aGVsbG8="));
    }

    #[test]
    fn copy_empty_string_does_not_panic() {
        let mut buf = Vec::new();
        copy_to_clipboard(&mut buf, "").unwrap();
    }

    #[test]
    fn copy_rejects_oversized_osc52_payloads() {
        let text = "x".repeat(MAX_OSC52_ENCODED_LENGTH / 4 * 3 + 3);
        let mut buf = Vec::new();
        let err = copy_to_clipboard(&mut buf, &text).unwrap_err();
        assert!(
            err.to_string().contains("too large"),
            "unexpected error: {err}"
        );
        assert!(buf.is_empty(), "nothing must be written on rejection");
    }

    #[test]
    fn linux_helpers_are_ordered_wayland_first() {
        assert_eq!(
            LINUX_COMMANDS.iter().map(|c| c.program).collect::<Vec<_>>(),
            vec!["wl-copy", "xclip", "xsel"]
        );
        assert_eq!(LINUX_COMMANDS[1].args, ["-selection", "clipboard"]);
        assert_eq!(LINUX_COMMANDS[2].args, ["--clipboard", "--input"]);
    }

    #[test]
    fn macos_and_windows_use_their_system_helper() {
        assert_eq!(MACOS_COMMANDS[0].program, "pbcopy");
        assert_eq!(WINDOWS_COMMANDS[0].program, "clip");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn platform_commands_follow_the_host() {
        assert_eq!(platform_commands()[0].program, "pbcopy");
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn platform_commands_follow_the_host() {
        assert_eq!(platform_commands()[0].program, "wl-copy");
    }

    #[test]
    fn first_working_helper_wins_and_stops_the_probe() {
        let runner = FakeRunner::new(&[], &[]);
        let probe = choose_channel("hi", false, LINUX_COMMANDS, &runner);
        assert_eq!(probe, Probe::Platform("wl-copy"));
        assert_eq!(runner.called(), vec!["wl-copy"], "no further probes");
        // The text is fed to the helper's stdin.
        assert_eq!(runner.calls.borrow()[0].2, "hi");
    }

    #[test]
    fn missing_and_failing_helpers_fall_through_to_the_next() {
        let runner = FakeRunner::new(&["wl-copy"], &["xclip"]);
        let probe = choose_channel("hi", false, LINUX_COMMANDS, &runner);
        assert_eq!(probe, Probe::Platform("xsel"));
        assert_eq!(runner.called(), vec!["wl-copy", "xclip", "xsel"]);
    }

    #[test]
    fn all_helpers_failing_falls_back_to_osc52() {
        let runner = FakeRunner::new(&["wl-copy", "xclip", "xsel"], &[]);
        assert_eq!(
            choose_channel("hi", false, LINUX_COMMANDS, &runner),
            Probe::Osc52
        );
        assert_eq!(runner.called().len(), 3, "every candidate was probed");
    }

    #[test]
    fn remote_session_skips_the_helpers() {
        // A local helper over SSH would fill the *remote* machine's clipboard.
        let runner = FakeRunner::new(&[], &[]);
        assert_eq!(
            choose_channel("hi", true, LINUX_COMMANDS, &runner),
            Probe::Osc52
        );
        assert!(runner.called().is_empty(), "no helper may be spawned");
    }

    #[test]
    fn remote_session_detection_reads_the_markers() {
        assert!(!is_remote_session_with(|_| None));
        assert!(!is_remote_session_with(|_| Some(String::new())));
        for marker in REMOTE_SESSION_MARKERS {
            assert!(
                is_remote_session_with(
                    |key| (key == marker).then(|| "10.0.0.1 22 10.0.0.2 55".to_string())
                ),
                "{marker} must mark the session as remote"
            );
        }
    }

    #[tokio::test]
    async fn platform_probe_result_does_not_write_osc52() {
        // The platform channel already delivered the text — writing OSC52 as
        // well would race the helper's writer for nothing.
        let mut buf = Vec::new();
        let path = finish_copy(&mut buf, "hello", Probe::Platform("pbcopy")).unwrap();
        assert_eq!(path, ClipboardPath::PlatformCommand("pbcopy"));
        assert!(buf.is_empty(), "no OSC52 bytes on the platform path");
    }

    #[tokio::test]
    async fn osc52_probe_result_writes_the_sequence() {
        let mut buf = Vec::new();
        let path = finish_copy(&mut buf, "hello", Probe::Osc52).unwrap();
        assert_eq!(path, ClipboardPath::Osc52);
        assert!(String::from_utf8(buf).unwrap().contains("aGVsbG8="));
    }
}
