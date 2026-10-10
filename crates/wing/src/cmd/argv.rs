//! Command-line intake: OS arguments → UTF-8 `String`s.
//!
//! `std::env::args()` **panics** on an argument that is not valid Unicode, and
//! on Unix an `OsString` argument is arbitrary bytes: one latin-1 path, one
//! stray byte in an orchestrator's shell variable, and the whole CLI died with
//! a Rust backtrace — before clap, before any subcommand, with nothing telling
//! the caller which argument was at fault. `args_os()` plus the check below
//! turn that into one readable stderr line and a documented exit code.
//!
//! The whole command line is decoded up front, in one place, so every path
//! (TUI, stdio, orchestration subcommands) inherits the same contract. That
//! includes arguments stdio mode would go on to *drop* (`filter_unknown_args`):
//! wing would rather fail loudly on a command line it cannot read than accept a
//! flag some later layer silently swallows — a dropped flag and a mangled one
//! look identical from the outside, and this diagnostic names the exact
//! argument, which is what the panic never did.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
use std::fmt::Write as _;

/// Exit code for a command line wing cannot decode.
///
/// 2 is what clap itself exits with on a usage error, so an orchestrator keeps
/// seeing one class of "you called me wrong" regardless of whether the bad part
/// was caught here or by the parser.
pub const EXIT_CODE_USAGE: u8 = 2;

/// Raw bytes included in the escaped preview of an offending argument.
///
/// A whole binary blob can arrive as a single argument (a shell inlining a
/// file's contents); stderr stays readable by capping the preview — the escape
/// below grows each byte to at most 4 characters — and reporting how much was
/// left out.
const PREVIEW_BYTES: usize = 96;

/// Decode the command line into UTF-8 `String`s.
///
/// `args` is the command line **without** the program name
/// (`std::env::args_os().skip(1)`), and [`NonUtf8Arg::position`] is the
/// argument's 1-based `argv` index — the position the caller would count to.
///
/// The first undecodable argument is returned as `Err` (fail fast: a second
/// message would not add anything). Note that the program name itself
/// (`argv[0]`) is never checked — it is the invoking process's business, and
/// wing does not use it.
pub fn decode_args<I>(args: I) -> Result<Vec<String>, NonUtf8Arg>
where
    I: IntoIterator<Item = OsString>,
{
    let mut decoded = Vec::new();
    for (index, arg) in args.into_iter().enumerate() {
        match arg.into_string() {
            Ok(text) => decoded.push(text),
            // `argv[index]` with `argv[0]` being the program name.
            Err(raw) => {
                return Err(NonUtf8Arg {
                    position: index + 1,
                    preview: escape_os_str(&raw),
                });
            }
        }
    }
    Ok(decoded)
}

/// An argument that is not valid UTF-8: where it sits on the command line and
/// what its bytes look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonUtf8Arg {
    /// 1-based `argv` index (`argv[0]` is the program name).
    pub position: usize,
    /// The argument's bytes, quoted and escaped (`"--bad=\xff"`); at most
    /// [`PREVIEW_BYTES`] raw bytes go in, so the escaped form is at most 4×
    /// that plus the quotes and the "more bytes" note.
    pub preview: String,
}

impl fmt::Display for NonUtf8Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "argument #{} is not valid UTF-8: {}; the whole command line must be UTF-8 text \
             (a mis-encoded path or shell variable is the usual cause)",
            self.position, self.preview
        )
    }
}

/// Render an OS argument as text that survives a terminal: printable ASCII
/// verbatim, every other byte as `\xNN` (high bytes included, so the *offending
/// bytes* are visible rather than collapsed into replacement characters).
fn escape_os_str(arg: &OsStr) -> String {
    let bytes = os_bytes(arg);
    let shown = bytes.len().min(PREVIEW_BYTES);
    let mut out = String::with_capacity(shown * 4 + 32);
    out.push('"');
    for &byte in &bytes[..shown] {
        push_escaped(&mut out, byte);
    }
    out.push('"');
    if bytes.len() > shown {
        // `write!` into a String is infallible.
        let _ = write!(out, " (+{} more bytes)", bytes.len() - shown);
    }
    out
}

fn push_escaped(out: &mut String, byte: u8) {
    match byte {
        b'"' => out.push_str("\\\""),
        b'\\' => out.push_str("\\\\"),
        0x20..=0x7e => out.push(byte as char),
        _ => {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
}

/// The raw bytes behind `arg`.
///
/// Unix keeps them verbatim. Elsewhere `OsString` is WTF-8/UTF-16, where a
/// non-UTF-8 argument is an unpaired surrogate: the lossy encoding is the best
/// available view, and its replacement characters still mark exactly where the
/// argument stops being UTF-8.
#[cfg(unix)]
fn os_bytes(arg: &OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(arg.as_bytes())
}

#[cfg(not(unix))]
fn os_bytes(arg: &OsStr) -> Cow<'_, [u8]> {
    Cow::Owned(arg.to_string_lossy().into_owned().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An argument decoded straight from raw bytes — the shape a shell hands
    /// over (`std::os::unix::ffi::OsStringExt` is Unix-only, hence the cfg on
    /// the cases that need it).
    #[cfg(unix)]
    fn raw(bytes: &[u8]) -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }

    fn os(text: &str) -> OsString {
        OsString::from(text)
    }

    #[test]
    fn valid_arguments_decode_in_order() {
        let decoded = decode_args([os("run"), os("--tag"), os("中文标签"), os("-p"), os("")])
            .expect("valid UTF-8 arguments must decode");
        assert_eq!(
            decoded,
            ["run", "--tag", "中文标签", "-p", ""],
            "arguments must survive decoding verbatim and in order"
        );
    }

    #[test]
    fn empty_command_line_decodes_to_nothing() {
        assert_eq!(decode_args(Vec::new()).unwrap(), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn invalid_argument_reports_its_argv_position_and_bytes() {
        let err = decode_args([os("-p"), os("hi"), raw(b"--tag=\xff")])
            .expect_err("an undecodable argument must be reported, not skipped");
        // `--tag=\xff` is argv[3]: `wing -p hi --tag=\xff`.
        assert_eq!(err.position, 3);
        assert_eq!(err.preview, "\"--tag=\\xff\"");
        assert_eq!(
            err.to_string(),
            "argument #3 is not valid UTF-8: \"--tag=\\xff\"; the whole command line must be \
             UTF-8 text (a mis-encoded path or shell variable is the usual cause)",
            "the message is the user-facing contract"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_first_undecodable_argument_is_the_one_reported() {
        let err = decode_args([raw(b"\xfe"), raw(b"\xff")]).expect_err("must fail");
        assert_eq!(err.position, 1, "fail fast on the earliest offender");
        assert_eq!(err.preview, "\"\\xfe\"");
    }

    #[cfg(unix)]
    #[test]
    fn preview_escapes_control_and_quote_bytes() {
        let err = decode_args([raw(b"a\0b\"c\\d\ne\xff")]).expect_err("must fail");
        assert_eq!(
            err.preview, "\"a\\x00b\\\"c\\\\d\\x0ae\\xff\"",
            "bytes outside printable ASCII must be escaped, quotes and backslashes included"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preview_is_capped_and_reports_the_remaining_bytes() {
        let mut bytes = vec![b'x'; PREVIEW_BYTES];
        bytes.extend_from_slice(&[0xff; 8]);
        let err = decode_args([raw(&bytes)]).expect_err("must fail");

        assert!(
            err.preview.ends_with("\" (+8 more bytes)"),
            "the tail must be summarised instead of dumped: {}",
            err.preview
        );
        assert!(
            err.preview.starts_with("\"xxxx"),
            "the head must still be shown: {}",
            err.preview
        );
        assert!(
            err.preview.len() < PREVIEW_BYTES * 2,
            "the preview must stay readable, got {} bytes",
            err.preview.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn valid_multibyte_arguments_are_not_rejected() {
        // Non-ASCII UTF-8 is perfectly legal input — only *undecodable* bytes
        // are refused.
        let decoded = decode_args([raw("路径/文件.txt".as_bytes()), raw("--模=值".as_bytes())])
            .expect("multi-byte UTF-8 must decode");
        assert_eq!(decoded, ["路径/文件.txt", "--模=值"]);
    }
}
