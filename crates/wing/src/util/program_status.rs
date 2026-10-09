//! Program status reporting via OSC 7501 — the Program Status Protocol.
//!
//! The protocol lets a program tell its terminal what it is doing — idle,
//! working, blocked on the user, done, or failed — so terminals and agent
//! dashboards ("inboxes") can show it without scraping the window title or
//! the screen. wing already writes the same states into the OSC 0 title for
//! the human in front of the tab ([`crate::util::title`]); this module is the
//! machine-readable counterpart for consumers that are *not* looking at the
//! screen: a terminal tab badge, an unfocused-tab spinner, a multi-agent
//! inbox.
//!
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>
//!
//! Like OSC 9, this is best-effort: a terminal that does not implement the
//! protocol must ignore unknown OSCs, so reports are safe to send without
//! probing support first (the spec permits it explicitly). `WING_PROGRAM_STATUS`
//! (`0` / `false` / `off`) switches reporting off.
//!
//! `msg` is shown by terminals and dashboards *outside* the grid — keep it a
//! short public summary (what the agent is working on / waiting for), never
//! raw session content.

use std::io::Write;

use anyhow::{Context, Result};

/// The record state (the spec's `state` key) plus `clear`, which removes the
/// record instead of reporting one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// At rest, waiting for the user's next instruction.
    Idle,
    /// Running.
    Working,
    /// Finished a piece of work; the result has not been seen yet.
    Done,
    /// Cannot continue until the user does something.
    Blocked(BlockKind),
    /// Failed and stopped.
    Error,
    /// Remove the record (and everything below it) instead of reporting one.
    Clear,
}

/// What a blocked program waits for (the `kind` key; only carried with
/// [`State::Blocked`] — the spec ignores it on other states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// Approval to do something (e.g. a dangerous Bash command).
    Permission,
    /// The user must type an answer.
    Question,
    /// A login, token, or credential. wing has no such flow today; the variant
    /// completes the spec's vocabulary.
    Auth,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked(_) => "blocked",
            Self::Error => "error",
            Self::Clear => "clear",
        }
    }
}

impl BlockKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }
}

/// Stable program name, sent as `app` on every report.
const APP: &str = "wing";

/// Decoded `msg` limit from the spec. Truncating before encoding keeps the
/// base64 under the spec's 2732-byte *encoded* limit (and the whole sequence
/// well under its 4096-byte cap).
const MAX_MSG_BYTES: usize = 2048;

/// Format one OSC 7501 report (root record, `app=wing`).
///
/// `msg` is sanitized (control characters become spaces — the spec discards a
/// report whose decoded text contains one), trimmed, and truncated to the
/// spec's decoded limit on a UTF-8 boundary before base64 encoding. A message
/// that ends up empty is omitted entirely.
pub fn format(state: State, msg: Option<&str>) -> String {
    let mut body = String::from("state=");
    body.push_str(state.as_str());
    if let State::Blocked(kind) = state {
        body.push_str(":kind=");
        body.push_str(kind.as_str());
    }
    body.push_str(":app=");
    body.push_str(APP);
    if let Some(msg) = msg {
        // Sanitize, then trim: a message that was entirely control characters
        // must not degenerate into a run of spaces.
        let text = sanitize(msg);
        let text = truncate_utf8(text.trim(), MAX_MSG_BYTES);
        if !text.is_empty() {
            body.push_str(":msg=");
            body.push_str(&base64_encode(text.as_bytes()));
        }
    }
    format!("\x1b]7501;{body}\x1b\\")
}

/// Write a report built by [`format`].
pub fn write(writer: &mut impl Write, report: &str) -> Result<()> {
    writer
        .write_all(report.as_bytes())
        .context("failed to write program status report")?;
    writer
        .flush()
        .context("failed to flush program status report")
}

/// Formats reports, drops repeats, and carries the `WING_PROGRAM_STATUS`
/// switch.
///
/// Records are "current state", not events: reporting the same record twice
/// is a no-op, so [`Reporter::report`] answers `None` for a report whose
/// exact sequence was just written. Comparison is on the formatted sequence —
/// what actually goes on the wire.
#[derive(Debug)]
pub struct Reporter {
    enabled: bool,
    last: Option<(State, String)>,
}

impl Reporter {
    /// A reporter with an explicit switch.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last: None,
        }
    }

    /// The production constructor: `WING_PROGRAM_STATUS=0|false|off` disables
    /// reporting; anything else (including unset) enables it.
    pub fn from_env() -> Self {
        Self::new(enabled_from_value(
            std::env::var("WING_PROGRAM_STATUS").ok().as_deref(),
        ))
    }

    /// The report to write, or `None` when reporting is off or the last
    /// written report was already exactly this one.
    pub fn report(&mut self, state: State, msg: Option<&str>) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let report = format(state, msg);
        if self.last.as_ref().is_some_and(|(_, last)| *last == report) {
            return None;
        }
        self.last = Some((state, report.clone()));
        Some(report)
    }

    /// Whether the last written record claims work that ends when the turn
    /// does (`working` / `blocked`). A reporter that never wrote — or is
    /// switched off — answers `false`.
    pub fn is_in_flight(&self) -> bool {
        matches!(
            self.last.as_ref(),
            Some((State::Working | State::Blocked(_), _))
        )
    }
}

fn enabled_from_value(value: Option<&str>) -> bool {
    !matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "off")
    )
}

/// Control characters become spaces: the spec discards a whole report whose
/// decoded text contains one, and the text is displayed outside the grid.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Truncate to at most `max_bytes`, never splitting a UTF-8 sequence.
fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Standard base64 (RFC 4648, padded). The spec allows omitting padding;
/// emitting it matches every other implementation.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = u32::from(chunk[0]);
        let b2 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b3 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encodes_utf8_bytes() {
        // "你好" (6 UTF-8 bytes) — multi-byte input must not be split.
        assert_eq!(base64_encode("你好".as_bytes()), "5L2g5aW9");
    }

    #[test]
    fn formats_working_without_message() {
        assert_eq!(
            format(State::Working, None),
            "\x1b]7501;state=working:app=wing\x1b\\"
        );
    }

    #[test]
    fn formats_blocked_with_kind_and_message() {
        assert_eq!(
            format(State::Blocked(BlockKind::Permission), Some("rm -rf /")),
            "\x1b]7501;state=blocked:kind=permission:app=wing:msg=cm0gLXJmIC8=\x1b\\"
        );
    }

    #[test]
    fn formats_every_state_and_kind() {
        assert_eq!(
            format(State::Idle, None),
            "\x1b]7501;state=idle:app=wing\x1b\\"
        );
        assert_eq!(
            format(State::Done, None),
            "\x1b]7501;state=done:app=wing\x1b\\"
        );
        assert_eq!(
            format(State::Error, None),
            "\x1b]7501;state=error:app=wing\x1b\\"
        );
        assert_eq!(
            format(State::Clear, None),
            "\x1b]7501;state=clear:app=wing\x1b\\"
        );
        for (kind, name) in [
            (BlockKind::Permission, "permission"),
            (BlockKind::Question, "question"),
            (BlockKind::Auth, "auth"),
        ] {
            assert_eq!(
                format(State::Blocked(kind), None),
                format!("\x1b]7501;state=blocked:kind={name}:app=wing\x1b\\")
            );
        }
    }

    #[test]
    fn message_control_characters_become_spaces() {
        // The bash confirmation question arrives with newlines and fences.
        assert_eq!(
            format(State::Blocked(BlockKind::Question), Some("a\nb\x07c")),
            "\x1b]7501;state=blocked:kind=question:app=wing:msg=YSBiIGM=\x1b\\"
        );
    }

    #[test]
    fn empty_message_is_omitted() {
        assert_eq!(format(State::Idle, Some("")), format(State::Idle, None));
        // A message that is nothing but control characters / whitespace must
        // not leave a dangling `msg=`.
        assert_eq!(
            format(State::Idle, Some("\n\x07")),
            format(State::Idle, None)
        );
        assert_eq!(
            format(State::Idle, Some("  \t\n")),
            format(State::Idle, None)
        );
    }

    #[test]
    fn message_truncates_on_utf8_boundary_at_the_spec_limit() {
        let long = "é".repeat(2000); // 4000 bytes
        let text = sanitize(&long);
        let text = truncate_utf8(&text, MAX_MSG_BYTES);
        assert_eq!(text.len(), MAX_MSG_BYTES, "truncated to the byte limit");
        assert!(text.chars().all(|c| c == 'é'), "never splits a code point");

        // Through `format`: encoded size stays under the spec's 2732-byte
        // limit and the whole sequence under its 4096-byte cap.
        let report = format(State::Done, Some(&long));
        let msg = report.split(":msg=").nth(1).unwrap();
        let encoded = msg.trim_end_matches("\x1b\\");
        assert_eq!(encoded.len(), 2732);
        assert!(report.len() <= 4096, "whole sequence within the spec cap");
    }

    #[test]
    fn reporter_dedupes_identical_reports() {
        let mut reporter = Reporter::new(true);
        assert!(reporter.report(State::Working, None).is_some());
        assert!(
            reporter.report(State::Working, None).is_none(),
            "same record"
        );
        assert!(
            reporter
                .report(State::Blocked(BlockKind::Question), Some("why?"))
                .is_some(),
            "a different record goes through"
        );
        // The (state, msg) pair formats to the same sequence as the last one.
        assert!(
            reporter
                .report(State::Blocked(BlockKind::Question), Some("why?"))
                .is_none()
        );
        assert!(reporter.report(State::Clear, None).is_some());
        // `clear` then `idle` are different records; both are written.
        assert!(reporter.report(State::Idle, None).is_some());
    }

    #[test]
    fn disabled_reporter_never_reports() {
        let mut reporter = Reporter::new(false);
        assert!(reporter.report(State::Working, None).is_none());
        assert!(reporter.report(State::Clear, None).is_none());
    }

    #[test]
    fn in_flight_tracks_the_last_written_record() {
        let mut reporter = Reporter::new(true);
        assert!(!reporter.is_in_flight(), "nothing written yet");
        reporter.report(State::Working, None);
        assert!(reporter.is_in_flight());
        reporter.report(State::Blocked(BlockKind::Permission), Some("approve?"));
        assert!(reporter.is_in_flight(), "blocked also ends with the turn");
        reporter.report(State::Done, Some("ok"));
        assert!(!reporter.is_in_flight());
        reporter.report(State::Error, None);
        assert!(!reporter.is_in_flight());
        reporter.report(State::Clear, None);
        assert!(!reporter.is_in_flight());
        // A disabled reporter never wrote anything, so nothing is in flight.
        let mut off = Reporter::new(false);
        assert!(off.report(State::Working, None).is_none());
        assert!(!off.is_in_flight());
    }

    #[test]
    fn env_switch_parsing() {
        assert!(enabled_from_value(None));
        assert!(enabled_from_value(Some("")));
        assert!(enabled_from_value(Some("1")));
        assert!(enabled_from_value(Some("yes")));
        assert!(!enabled_from_value(Some("0")));
        assert!(!enabled_from_value(Some("false")));
        assert!(!enabled_from_value(Some(" FALSE ")));
        assert!(!enabled_from_value(Some("off")));
    }

    #[test]
    fn write_emits_the_report() {
        let mut buf = Vec::new();
        let report = format(State::Done, Some("ok"));
        write(&mut buf, &report).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), report);
    }
}
