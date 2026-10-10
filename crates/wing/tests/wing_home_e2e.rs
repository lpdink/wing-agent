//! End-to-end: every reader derives its path from `$WING_HOME`.
//!
//! The frontend and the backend read the same variable and must land in the
//! same directory. Python reaches it through `os.environ` (decoded with
//! `surrogateescape`, so the real bytes survive); Rust's `std::env::var` returns
//! `Err` for a non-UTF-8 value, which the CLI used to swallow — quietly falling
//! back to `~/.wing`, with nothing on stderr and config / logs / sessions split
//! across two homes.
//!
//! The three readers: the backend config path (and everything else under the
//! root, `cmd::backend_config`), the frontend log directory (`util::logging`)
//! and the TUI config file (`config::store`). `$HOME` is always redirected into
//! a scratch directory, so the assertions never get near the real `~/.wing`.

#![cfg(unix)]

use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// A scratch `$WING_HOME`, removed on drop.
///
/// `bytes` widens the name with a suffix of raw bytes, so a test can hand the
/// child a home that is not valid UTF-8. Only the *path* is built here: APFS
/// (macOS) refuses to create a name that is not valid UTF-8, so whether a byte
/// home can exist as a real directory is the filesystem's business — the one
/// case that needs it is Linux-gated below.
struct ScratchHome {
    path: PathBuf,
}

impl ScratchHome {
    /// `tag` keeps concurrent tests (and their `$WING_HOME`) apart; `suffix` is
    /// appended as raw bytes, so a test can hand the child a home that is not
    /// valid UTF-8.
    ///
    /// Only the *path* is built: whether it can be materialised is the
    /// filesystem's business (APFS rejects non-UTF-8 names), so the cases that
    /// need a real directory create it themselves.
    fn new(tag: &str, suffix: &[u8]) -> Self {
        let base = std::env::temp_dir().join(format!("wing-e2e-home-{}-{tag}", std::process::id()));
        let mut name: Vec<u8> = base.into_os_string().into_vec();
        name.extend_from_slice(suffix);
        let path = PathBuf::from(OsString::from_vec(name));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }

    /// A home whose name carries an invalid UTF-8 byte (`…-ho\xffme`).
    fn undecodable(tag: &str) -> Self {
        Self::new(tag, b"-ho\xffme")
    }

    /// Create the home directory itself (only for names the filesystem accepts).
    #[cfg(target_os = "linux")]
    fn create(&self) {
        std::fs::create_dir_all(&self.path).expect("create scratch home");
    }

    /// The home as the OS hands it over — the value the child gets in its env.
    fn os(&self) -> OsString {
        self.path.clone().into_os_string()
    }

    fn join(&self, tail: &str) -> PathBuf {
        self.path.join(tail)
    }

    fn write(&self, tail: &str, content: &str) {
        let path = self.join(tail);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, content).expect("write into scratch home");
    }
}

impl Drop for ScratchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawn the real binary with `$WING_HOME` set to the scratch home, and `$HOME`
/// pointed at it too (so the fallback branches stay in the scratch directory).
async fn run(home: &ScratchHome, args: &[&str]) -> Output {
    let home_env = home.os();
    run_with(
        &std::env::temp_dir(),
        args,
        &[
            ("WING_HOME", home_env.as_os_str()),
            ("HOME", home_env.as_os_str()),
        ],
    )
    .await
}

/// Spawn the real binary from `cwd` with exactly the environment given.
async fn run_with(cwd: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        command.env(key, value);
    }
    let child = command.spawn().expect("spawn wing");
    // Every command here is local — nothing may need a gateway or a real home.
    match tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await {
        Ok(result) => result.expect("wait for wing"),
        Err(_) => panic!("`wing {}` did not exit", args.join(" ")),
    }
}

#[tokio::test]
async fn the_backend_config_path_lives_in_the_given_home() {
    let home = ScratchHome::new("config-path", b"");
    let output = run(&home, &["config", "path"]).await;

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "`wing config path` is local and must succeed, got {:?}\nstderr: {stderr}",
        output.status
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        home.join("core/config.yaml").to_string_lossy().as_ref(),
    );
}

#[tokio::test]
async fn the_log_directory_lives_in_the_given_home() {
    let home = ScratchHome::new("logs", b"");
    let output = run(&home, &["config", "path"]).await;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let logs = home.join("tui/logs");
    assert!(
        logs.is_dir(),
        "`wing` must log under `$WING_HOME/tui/logs` — `{}` is missing",
        logs.display()
    );
}

#[tokio::test]
async fn the_tui_config_comes_from_the_given_home() {
    let home = ScratchHome::new("tui-config", b"");
    // Not YAML: the reader must fail on *this* file, and name it.
    home.write("tui/config.yaml", "[");

    let output = run(&home, &["tui", "--dump-config"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "a broken config must fail the dump, got {:?}\nstderr: {stderr}",
        output.status
    );
    assert!(
        stderr.contains("cannot parse"),
        "the failure must be the parse error of this file: {stderr}"
    );
    let shown = home.join("tui/config.yaml").to_string_lossy().to_string();
    assert!(
        stderr.contains(&shown),
        "stderr must name the file it read (`{shown}`): {stderr}"
    );
}

/// A home that is not valid UTF-8 is used *as it is*.
///
/// The path itself never needs to exist for this: `wing config path` only joins
/// path components. What this pins is the *fallback* — the byte home must not be
/// discarded in favour of `~/.wing`. Byte fidelity itself is pinned in
/// `util::wing_home`'s unit test (raw bytes in, raw bytes out); the stdout here
/// is inert either way, since a path is only ever *displayed* lossily
/// (`cmd::config` prints `to_string_lossy()`, so `\xff` and a literal `U+FFFD`
/// look the same).
#[tokio::test]
async fn a_non_utf8_home_is_used_as_it_is() {
    let home = ScratchHome::undecodable("bytes");
    let output = run(&home, &["config", "path"]).await;

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "got {:?}\nstderr: {stderr}",
        output.status
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        home.join("core/config.yaml").to_string_lossy().as_ref(),
        "the byte path must be used, not silently replaced by ~/.wing"
    );
}

#[tokio::test]
async fn an_empty_home_falls_back_to_the_environment_home() {
    // `$WING_HOME=""` means "unset" (the backend's `get_wing_home()` reads it the
    // same way). Without that, the value used to be taken literally and the
    // config path became a *relative* `core/config.yaml` under the cwd — while
    // the backend happily used `~/.wing`.
    let home = ScratchHome::new("empty", b"");
    let home_env = home.os();
    let output = run_with(
        &std::env::temp_dir(),
        &["config", "path"],
        &[
            ("WING_HOME", OsStr::new("")),
            ("HOME", home_env.as_os_str()),
        ],
    )
    .await;

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "got {:?}\nstderr: {stderr}",
        output.status
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        home.join(".wing/core/config.yaml")
            .to_string_lossy()
            .as_ref(),
        "an empty `$WING_HOME` must mean `~/.wing`, the same as the backend"
    );
}

#[tokio::test]
async fn a_tilde_home_expands_to_the_environment_home() {
    // The backend expands a leading `~` (`Path.expanduser()`); a literal reading
    // would put the frontend in a directory *named* `~` inside the cwd.
    let home = ScratchHome::new("tilde", b"");
    let cwd = home.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create scratch cwd");
    let home_env = home.os();
    let output = run_with(
        &cwd,
        &["config", "path"],
        &[
            ("WING_HOME", OsStr::new("~/wing")),
            ("HOME", home_env.as_os_str()),
        ],
    )
    .await;

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "got {:?}\nstderr: {stderr}",
        output.status
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        home.join("wing/core/config.yaml")
            .to_string_lossy()
            .as_ref(),
        "`~/wing` must expand against `$HOME`, exactly like the backend"
    );
    assert!(
        !cwd.join("~").exists(),
        "a literal `~` directory must never appear in the cwd: {}",
        cwd.join("~").display()
    );
}

/// The same, with the directory actually materialised: a byte home can only be
/// created where the filesystem allows the name (Linux; CI runs ubuntu), and
/// then the log directory has to appear inside it — with the *bytes* intact in
/// the log line that names it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_non_utf8_home_receives_the_logs() {
    let home = ScratchHome::undecodable("bytes-logs");
    home.create();
    let home_env = home.os();
    // `wing=info` so the line carrying the resolved log directory is written.
    let output = run_with(
        &std::env::temp_dir(),
        &["config", "path"],
        &[
            ("WING_HOME", home_env.as_os_str()),
            ("RUST_LOG", OsStr::new("wing=info")),
        ],
    )
    .await;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let logs = home.join("tui/logs");
    assert!(
        logs.is_dir(),
        "`{}` is missing — the byte home was not the one in use",
        logs.display()
    );
    // The log file proves byte fidelity: the directory it names renders as
    // `\xff` (an escaped byte), not as the lossy replacement character.
    let log = std::fs::read_dir(&logs)
        .expect("read log dir")
        .map(|entry| entry.expect("log entry").path())
        .find(|path| path.extension().is_some_and(|ext| ext == "log"))
        .expect("a log file was written");
    let text = std::fs::read_to_string(&log).expect("read log file");
    assert!(
        text.to_lowercase().contains("\\xff"),
        "the log must name the byte directory as bytes, not lossily: {text}"
    );
}
