//! Opening links: classify a markdown destination, resolve it, hand it to a
//! system opener.
//!
//! The whole module is argv-only on purpose: a link destination is *model
//! output*, so it must never reach a shell. [`OpenPlan`] carries a program plus
//! an argument vector (`OsString`s), and the only process we start is that
//! program — no `sh -c`, no string concatenation, no globbing.
//!
//! Resolution rules (see the `tui-link-open` change):
//!
//! * a destination with a scheme (`https:`, `mailto:`, …) is a URL, opened by
//!   the platform's default browser launcher;
//! * a destination that looks like a path (`/abs`, `./rel`, `../rel`, `~/rel`,
//!   `file://…`, a drive letter, a UNC path) — or a bare relative form — is a
//!   local file, resolved against `$HOME` (`~`), the **CLI launch directory**
//!   (`current_dir`, *not* the session workspace) and checked for existence
//!   before anything is started;
//! * a `#L10` / `#L10C5` / `:10` / `:10:5` suffix on a local target is a line
//!   (and column) reference: it is stripped from the path and, when a
//!   line-capable editor CLI is on `PATH`, used to jump to that line.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

use crate::render::markdown::links::extract_hidden_location_suffix;
use crate::render::markdown::links::is_local_path_like_link;

/// How long we are willing to wait for the opener to return.
///
/// `open` / `xdg-open` hand off in well under a second; a launcher that has not
/// returned by then is either wedged or waiting on something we cannot see, and
/// blocking the event loop for it would be worse than not knowing (the run loop
/// awaits this intent serially — see `app::runner`).
const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A parsed link destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget {
    /// A remote / scheme-carrying target, passed to the opener verbatim.
    Url(String),
    /// A local file. `line` / `column` are the 1-based location suffix, if any.
    File {
        path: PathBuf,
        line: Option<u32>,
        column: Option<u32>,
    },
}

/// The exact program + argv to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlan {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// Human-readable description for logs (never executed).
    pub describe: String,
}

/// Plausible editor CLIs that accept a `path:line[:col]` argument.
///
/// The platform *default* program cannot jump to a line, so a suffix can only
/// be honoured through a known editor. Declared for every platform (not just
/// the host) so the table is unit-testable wherever the tests run.
#[allow(dead_code)]
const GUI_EDITORS: &[(&str, &str)] = &[("code", "-g"), ("subl", ""), ("zed", "")];

/// The platform's "open this with the default application" launcher.
struct PlatformOpener {
    program: &'static str,
    /// Arguments inserted before the target (`cmd /c start "" <target>` needs
    /// the empty title argument; the others take the target directly).
    prefix: &'static [&'static str],
}

#[allow(dead_code)]
const MACOS_OPENER: PlatformOpener = PlatformOpener {
    program: "open",
    prefix: &[],
};

#[allow(dead_code)]
const WINDOWS_OPENER: PlatformOpener = PlatformOpener {
    program: "cmd",
    prefix: &["/c", "start", ""],
};

#[allow(dead_code)]
const LINUX_OPENER: PlatformOpener = PlatformOpener {
    program: "xdg-open",
    prefix: &[],
};

fn platform_opener() -> &'static PlatformOpener {
    #[cfg(target_os = "macos")]
    {
        &MACOS_OPENER
    }
    #[cfg(target_os = "windows")]
    {
        &WINDOWS_OPENER
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        &LINUX_OPENER
    }
}

/// The environment the resolution depends on — injected so tests never touch
/// the real one.
#[derive(Debug, Clone)]
pub struct Env {
    /// `$HOME` (or the platform equivalent) — `~` expansion.
    pub home: Option<PathBuf>,
    /// The CLI's launch directory: the base for relative targets.
    pub cwd: PathBuf,
    /// `$PATH` entries, in order.
    pub path: Vec<PathBuf>,
}

impl Env {
    /// Read the real process environment.
    pub fn from_process() -> Self {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let cwd = std::env::current_dir().unwrap_or_else(|e| {
            tracing::warn!("cannot read the current directory: {e}");
            PathBuf::from(".")
        });
        let path = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        Self { home, cwd, path }
    }

    /// Whether `program` resolves to an executable we could run.
    fn has_executable(&self, program: &str) -> bool {
        if program.contains(std::path::MAIN_SEPARATOR) {
            return PathBuf::from(program).is_file();
        }
        self.path.iter().any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file() && is_executable(&candidate)
        })
    }
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

// ============================================================
// Classification
// ============================================================

/// URL scheme of `raw`, if it has one (`https`, `mailto`, …).
///
/// A single letter followed by `:` is a Windows drive (`C:\x`), not a scheme.
fn url_scheme(raw: &str) -> Option<&str> {
    let colon = raw.find(':')?;
    let scheme = &raw[..colon];
    if scheme.len() < 2 || !scheme.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        .then_some(scheme)
}

/// Whether `raw` should be treated as a local path.
///
/// Everything that is not a URL is a path: a bare relative form (`src/main.rs`)
/// is far more likely in a coding session than a scheme-less host name, and a
/// missing file fails loudly instead of handing garbage to the browser.
fn looks_like_path(raw: &str) -> bool {
    is_local_path_like_link(raw) || url_scheme(raw).is_none()
}

/// Parse a link destination into a [`OpenTarget`] (no file-system access).
pub fn classify(raw: &str) -> Option<OpenTarget> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if looks_like_path(raw) {
        let (path_part, line, column) = split_location_suffix(raw);
        let path = path_part?;
        return Some(OpenTarget::File { path, line, column });
    }
    Some(OpenTarget::Url(raw.to_string()))
}

/// Strip the location suffix and turn it into `(line, column)`.
fn split_location_suffix(raw: &str) -> (Option<PathBuf>, Option<u32>, Option<u32>) {
    let (stripped, line, column) = match extract_hidden_location_suffix(raw) {
        Some(suffix) => {
            let body = &raw[..raw.len() - suffix.len()];
            parse_location_suffix(&suffix)
                .map(|(line, column)| (Some(body), Some(line), column))
                .unwrap_or((Some(raw), None, None))
        }
        None => (Some(raw), None, None),
    };
    let Some(path) = stripped else {
        return (None, None, None);
    };
    // `file://` (with an optional `localhost` authority) is a plain path.
    let path = path
        .strip_prefix("file://localhost")
        .or_else(|| path.strip_prefix("file://"))
        .unwrap_or(path);
    (Some(PathBuf::from(path)), line, column)
}

/// `#L10` / `#L10C5` / `:10` / `:10:5` → `(line, column)`.
fn parse_location_suffix(suffix: &str) -> Option<(u32, Option<u32>)> {
    let body = suffix.strip_prefix('#').unwrap_or(suffix);
    if let Some(rest) = body.strip_prefix(['L', 'l']) {
        let (line, column) = match rest.split_once(['C', 'c']) {
            Some((line, col)) => (line, Some(col)),
            None => (rest, None),
        };
        return Some((line.parse().ok()?, column.map(str::parse).transpose().ok()?));
    }
    // `:10` or `:10:5` — the suffix starts with the colon.
    let body = body.strip_prefix(':').unwrap_or(body);
    let (line, column) = match body.split_once(':') {
        Some((line, col)) => (line, Some(col)),
        None => (body, None),
    };
    Some((line.parse().ok()?, column.map(str::parse).transpose().ok()?))
}

// ============================================================
// Planning
// ============================================================

/// Resolve `raw`, check that a local target exists and pick the argv to run.
///
/// Pure with respect to the process table: the only side effect is reading
/// `Env`. A `None`/`Err` means "do not start anything".
pub fn plan(raw: &str, env: &Env) -> Result<OpenPlan> {
    let target = classify(raw).with_context(|| format!("unusable link target: {raw:?}"))?;
    match target {
        OpenTarget::Url(url) => Ok(url_plan(&url)),
        OpenTarget::File { path, line, column } => {
            let path = resolve_path(&path, env)?;
            if !path.exists() {
                bail!("no such file: {}", path.display());
            }
            Ok(file_plan(&path, line, column, env))
        }
    }
}

/// Expand `~`, anchor relative paths at the launch directory, normalise.
fn resolve_path(path: &std::path::Path, env: &Env) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    let expanded: PathBuf = if raw == "~" {
        env.home
            .clone()
            .context("cannot expand `~`: no home directory")?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        env.home
            .clone()
            .context("cannot expand `~`: no home directory")?
            .join(rest)
    } else {
        path.to_path_buf()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        // Relative to where the CLI was started — never the session workspace:
        // an implicit directory would open a different file than the one the
        // path visibly points at.
        env.cwd.join(expanded)
    };
    // A dangling symlink or a `..`-heavy path still opens, but the reported
    // path should be the real one when we can get it.
    Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
}

fn url_plan(url: &str) -> OpenPlan {
    let opener = platform_opener();
    OpenPlan {
        program: OsString::from(opener.program),
        args: opener
            .prefix
            .iter()
            .map(OsString::from)
            .chain(std::iter::once(OsString::from(url)))
            .collect(),
        describe: format!("{} {url}", opener.program),
    }
}

fn file_plan(
    path: &std::path::Path,
    line: Option<u32>,
    column: Option<u32>,
    env: &Env,
) -> OpenPlan {
    if let Some(line) = line {
        for (program, flag) in GUI_EDITORS {
            if !env.has_executable(program) {
                continue;
            }
            let location = match column {
                Some(column) => format!("{}:{line}:{column}", path.display()),
                None => format!("{}:{line}", path.display()),
            };
            let mut args: Vec<OsString> = Vec::new();
            if !flag.is_empty() {
                args.push(OsString::from(*flag));
            }
            args.push(OsString::from(location));
            return OpenPlan {
                program: OsString::from(*program),
                args,
                describe: format!("{program} → {} line {line}", path.display()),
            };
        }
        tracing::debug!(
            "no line-capable editor on PATH; opening {} without the line jump",
            path.display()
        );
    }
    let opener = platform_opener();
    OpenPlan {
        program: OsString::from(opener.program),
        args: opener
            .prefix
            .iter()
            .map(OsString::from)
            .chain(std::iter::once(path.as_os_str().to_owned()))
            .collect(),
        describe: format!("{} {}", opener.program, path.display()),
    }
}

// ============================================================
// Running
// ============================================================

/// Starts one [`OpenPlan`]. Injected so tests assert argv without spawning.
pub(crate) trait OpenerRunner {
    fn run(&self, plan: &OpenPlan) -> io::Result<()>;
}

/// Production runner: `Command` with captured stdio.
///
/// The TUI owns the terminal in raw mode, so the child must never inherit it
/// (stdout/stderr are piped and captured — the crate also denies direct
/// printing). No shell is involved: `program` is exec'd with `args` verbatim.
pub(crate) struct ProcessRunner;

impl OpenerRunner for ProcessRunner {
    fn run(&self, plan: &OpenPlan) -> io::Result<()> {
        let output = std::process::Command::new(&plan.program)
            .args(&plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        Err(io::Error::other(if detail.is_empty() {
            format!(
                "{} exited with {}",
                plan.program.to_string_lossy(),
                output.status
            )
        } else {
            format!(
                "{} exited with {}: {detail}",
                plan.program.to_string_lossy(),
                output.status
            )
        }))
    }
}

/// Plan with `env` and run the result with `runner` (synchronous — the async
/// entry point wraps this in the blocking pool).
pub(crate) fn open_with(runner: &dyn OpenerRunner, raw: &str, env: &Env) -> Result<OpenPlan> {
    let plan = plan(raw, env)?;
    runner
        .run(&plan)
        .with_context(|| format!("opening {} failed", plan.describe))?;
    Ok(plan)
}

/// Open a link target with the system opener (default application / browser).
///
/// Resolves and launches on the blocking pool: spawning a process must never
/// stall the event loop or a frame. Returns the plan that was actually run so
/// the caller can log it.
pub async fn open_target(raw: &str) -> Result<OpenPlan> {
    let owned = raw.to_owned();
    let handle = tokio::task::spawn_blocking(move || {
        let env = Env::from_process();
        open_with(&ProcessRunner, &owned, &env)
    });
    match tokio::time::timeout(OPEN_TIMEOUT, handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(anyhow::anyhow!("opener task failed: {join}")),
        Err(_) => bail!(
            "opener did not return within {} ms",
            OPEN_TIMEOUT.as_millis()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// The temp dir is a symlink on macOS — resolution canonicalises, so the
    /// expectations must too.
    fn canon(path: &std::path::Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn env_with(path: &[&str]) -> Env {
        Env {
            home: Some(PathBuf::from("/home/tester")),
            cwd: PathBuf::from("/work/proj"),
            path: path.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn schemes_are_urls_and_drive_letters_are_paths() {
        assert_eq!(
            classify("https://example.com/a#L10"),
            Some(OpenTarget::Url("https://example.com/a#L10".into())),
            "a URL fragment must survive"
        );
        assert!(matches!(classify("mailto:x@y.z"), Some(OpenTarget::Url(_))));
        assert!(matches!(
            classify("C:\\work\\main.rs"),
            Some(OpenTarget::File { .. })
        ));
        assert!(matches!(
            classify("\\\\\\\\server\\\\share\\\\x"),
            Some(OpenTarget::File { .. })
        ));
    }

    #[test]
    fn local_forms_are_files_with_the_suffix_split_off() {
        for (raw, path, line, column) in [
            ("./src/main.rs", "./src/main.rs", None, None),
            ("./src/main.rs#L10", "./src/main.rs", Some(10), None),
            ("./src/main.rs#L10C5", "./src/main.rs", Some(10), Some(5)),
            ("./src/main.rs:10", "./src/main.rs", Some(10), None),
            ("./src/main.rs:10:5", "./src/main.rs", Some(10), Some(5)),
            ("~/notes.md", "~/notes.md", None, None),
            ("/abs/file.rs", "/abs/file.rs", None, None),
            ("file:///tmp/x.rs", "/tmp/x.rs", None, None),
            ("file://localhost/tmp/x.rs", "/tmp/x.rs", None, None),
            ("src/main.rs", "src/main.rs", None, None),
        ] {
            match classify(raw) {
                Some(OpenTarget::File {
                    path: got,
                    line: got_line,
                    column: got_column,
                }) => {
                    assert_eq!(got, PathBuf::from(path), "path of {raw}");
                    assert_eq!(got_line, line, "line of {raw}");
                    assert_eq!(got_column, column, "column of {raw}");
                }
                other => panic!("{raw} classified as {other:?}"),
            }
        }
    }

    #[test]
    fn empty_targets_are_rejected() {
        assert_eq!(classify("   "), None);
        assert!(plan("", &env_with(&[])).is_err());
    }

    #[test]
    fn relative_paths_are_anchored_at_the_launch_directory() {
        let env = env_with(&[]);
        let resolved = resolve_path(std::path::Path::new("src/main.rs"), &env).unwrap();
        assert_eq!(resolved, PathBuf::from("/work/proj/src/main.rs"));
        let resolved = resolve_path(std::path::Path::new("./src/../main.rs"), &env).unwrap();
        assert_eq!(resolved, PathBuf::from("/work/proj/src/../main.rs"));
    }

    #[test]
    fn tilde_expands_to_the_home_directory() {
        let env = env_with(&[]);
        assert_eq!(
            resolve_path(std::path::Path::new("~/notes.md"), &env).unwrap(),
            PathBuf::from("/home/tester/notes.md")
        );
        assert_eq!(
            resolve_path(std::path::Path::new("~"), &env).unwrap(),
            PathBuf::from("/home/tester")
        );
        let mut homeless = env.clone();
        homeless.home = None;
        assert!(resolve_path(std::path::Path::new("~/x"), &homeless).is_err());
    }

    #[test]
    fn a_missing_file_never_reaches_the_runner() {
        #[derive(Default)]
        struct PanicRunner {
            calls: RefCell<usize>,
        }
        impl OpenerRunner for PanicRunner {
            fn run(&self, plan: &OpenPlan) -> io::Result<()> {
                *self.calls.borrow_mut() += 1;
                panic!("must not run {plan:?}");
            }
        }
        let runner = PanicRunner::default();
        let err = open_with(&runner, "./definitely/not/here.rs", &env_with(&[])).unwrap_err();
        assert!(err.to_string().contains("no such file"), "{err}");
        assert_eq!(*runner.calls.borrow(), 0);
    }

    #[test]
    fn url_plans_carry_the_target_as_one_argument() {
        let env = env_with(&[]);
        let plan = plan("https://example.com/a;b`c|d$(e)", &env).unwrap();
        assert_eq!(plan.program, OsString::from(platform_opener().program));
        assert_eq!(
            plan.args.last().unwrap(),
            &OsString::from("https://example.com/a;b`c|d$(e)"),
            "shell metacharacters must stay inside a single argv element"
        );
        assert_eq!(
            plan.args.len(),
            platform_opener().prefix.len() + 1,
            "no extra arguments are synthesised"
        );
    }

    #[test]
    fn file_plans_use_the_line_capable_editor_when_it_exists() {
        let dir = std::env::temp_dir().join(format!("wing-open-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let code = dir.join("code");
        std::fs::write(&code, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&code, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut env = env_with(&[]);
        env.path = vec![dir.clone()];
        let raw = format!("{}#L12C3", file.display());
        let with_editor = plan(&raw, &env).unwrap();
        assert_eq!(with_editor.program, OsString::from("code"));
        assert_eq!(with_editor.args[0], OsString::from("-g"));
        assert_eq!(
            with_editor.args[1],
            OsString::from(format!("{}:12:3", canon(&file).display()))
        );

        // Without the editor on PATH the platform opener takes over and the
        // line is dropped (never invented).
        let bare = env_with(&[]);
        let without_editor = plan(&raw, &bare).unwrap();
        assert_eq!(
            without_editor.program,
            OsString::from(platform_opener().program)
        );
        assert_eq!(
            without_editor.args.last().unwrap(),
            &canon(&file).into_os_string()
        );

        // No suffix → straight to the platform opener even with the editor.
        let no_suffix = plan(&file.display().to_string(), &env).unwrap();
        assert_eq!(no_suffix.program, OsString::from(platform_opener().program));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn runner_receives_the_planned_argv() {
        struct RecordingRunner {
            seen: RefCell<Vec<OpenPlan>>,
        }
        impl OpenerRunner for RecordingRunner {
            fn run(&self, plan: &OpenPlan) -> io::Result<()> {
                self.seen.borrow_mut().push(plan.clone());
                Ok(())
            }
        }
        let dir = std::env::temp_dir().join(format!("wing-open-argv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.txt");
        std::fs::write(&file, "x").unwrap();

        let runner = RecordingRunner {
            seen: RefCell::new(Vec::new()),
        };
        let raw = file.display().to_string();
        let plan = open_with(&runner, &raw, &env_with(&[])).unwrap();
        let seen = runner.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], plan);
        assert_eq!(seen[0].args.last().unwrap(), &canon(&file).into_os_string());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failing_runner_is_reported_with_the_program_name() {
        struct FailRunner;
        impl OpenerRunner for FailRunner {
            fn run(&self, _plan: &OpenPlan) -> io::Result<()> {
                Err(io::Error::other("exited with exit status: 3"))
            }
        }
        let dir = std::env::temp_dir().join(format!("wing-open-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let err = open_with(&FailRunner, &file.display().to_string(), &env_with(&[])).unwrap_err();
        // `{:#}` walks the anyhow chain: context + the runner's reason.
        let text = format!("{err:#}");
        assert!(text.contains("exit status: 3"), "{text}");
        assert!(text.contains(platform_opener().program), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
