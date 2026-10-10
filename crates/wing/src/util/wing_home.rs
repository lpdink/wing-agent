//! `$WING_HOME` — the root the frontend and the backend must agree on.
//!
//! Both sides read the same variable and must land on the same directory. The
//! backend reads it through `os.environ` (Python decodes with `surrogateescape`,
//! so the real bytes survive — `libs/core/wing/config/loader.py::get_wing_home`);
//! the frontend reads it here through `var_os`. `std::env::var` would instead
//! drop a non-UTF-8 value on the floor and silently fall back to `~/.wing`: two
//! home directories, no diagnostic, config / logs / sessions split in half.
//!
//! The rule matches the backend's: an **unset or empty** value means `~/.wing`,
//! and a leading `~` expands to the home directory (`Path.expanduser()`),
//! because `WING_HOME=~/wing` has to mean one directory on both sides. A
//! `~user` form is the one spelling left unexpanded — resolving another user's
//! home needs `getpwnam`, which `std` does not expose.

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

/// `$WING_HOME` as given, or `None` when it is unset or empty.
///
/// The value is kept byte-for-byte (`OsString` → `PathBuf`): a home directory
/// is not required to be valid UTF-8, and quietly looking at a *different*
/// directory is not a fallback anyone asked for.
pub fn env_override() -> Option<PathBuf> {
    resolve(std::env::var_os("WING_HOME"))
}

/// `~/.wing`, or `None` when the home directory itself is unknown.
pub fn home_default() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".wing"))
}

/// The wing root: `$WING_HOME`, else `~/.wing` (`./.wing` when even that is
/// unknown).
pub fn root() -> PathBuf {
    resolve_root(std::env::var_os("WING_HOME"))
}

/// The rules above, as pure functions of what the environment holds: env reads
/// are process-global and racy under parallel tests, so the tests pin these.
fn resolve(home: Option<OsString>) -> Option<PathBuf> {
    home.filter(|value| !value.is_empty()).map(expand_tilde)
}

fn resolve_root(home_env: Option<OsString>) -> PathBuf {
    resolve(home_env)
        .or_else(home_default)
        .unwrap_or_else(|| PathBuf::from(".").join(".wing"))
}

/// Expand a leading `~` (`~`, `~/…`) into the home directory.
///
/// The backend does this (`Path(env_home).expanduser()`), so without it
/// `WING_HOME=~/wing` would put the frontend in a directory named `~` — inside
/// whichever cwd it happened to have — while the backend used the real home:
/// the same two-homes split this module exists to prevent.
///
/// Anything else is returned as written: `~user` (no `getpwnam` in `std`), a
/// `~` that is not the first component, a value that is not UTF-8, and the case
/// where there is no home directory to expand to at all.
fn expand_tilde(value: OsString) -> PathBuf {
    let path = PathBuf::from(value);
    let Ok(remainder) = path.strip_prefix("~").map(Path::to_path_buf) else {
        return path;
    };
    let Some(home) = dirs::home_dir() else {
        return path;
    };
    if remainder.as_os_str().is_empty() {
        home
    } else {
        home.join(remainder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home directory as the OS hands it over: raw bytes.
    #[cfg(unix)]
    fn raw(bytes: &[u8]) -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }

    #[test]
    fn absent_or_empty_means_no_override() {
        assert_eq!(resolve(None), None);
        assert_eq!(resolve(Some(OsString::new())), None);
        // Both branches land on the same root: an empty variable must not turn
        // into a (relative!) empty path while the backend reads `~/.wing`.
        assert_eq!(
            resolve_root(Some(OsString::new())),
            resolve_root(None),
            "empty `$WING_HOME` behaves exactly like an unset one"
        );
    }

    #[test]
    fn a_value_is_used_as_given() {
        assert_eq!(
            resolve(Some(OsString::from("/srv/wing"))),
            Some(PathBuf::from("/srv/wing"))
        );
        assert_eq!(
            resolve_root(Some(OsString::from("/srv/wing"))),
            PathBuf::from("/srv/wing"),
            "an override wins over the home directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_value_keeps_its_bytes() {
        // The backend uses this directory (Python decodes the same bytes), so
        // falling back to `~/.wing` here would split the two halves of wing.
        let home = resolve(Some(raw(b"/tmp/\xffwing"))).expect("a non-empty value is an override");
        assert_eq!(
            home,
            PathBuf::from(raw(b"/tmp/\xffwing")),
            "the path must survive byte-for-byte"
        );
        assert!(
            home.to_string_lossy().contains('\u{fffd}'),
            "and it is only the *display* that is lossy: {home:?}"
        );
    }

    #[test]
    fn root_is_always_usable() {
        // `resolve_root` always produces a path — an unknown home directory
        // degrades to `./.wing` rather than to an empty (relative) root.
        assert_eq!(
            resolve_root(None),
            home_default().unwrap_or_else(|| PathBuf::from(".").join(".wing"))
        );
        assert!(!resolve_root(None).as_os_str().is_empty());
        assert!(
            !root().as_os_str().is_empty(),
            "whatever this process inherits, there is always a wing root"
        );
    }

    #[test]
    fn a_leading_tilde_expands_to_the_home_directory() {
        // The backend expands it (`Path.expanduser()`), so both sides must.
        let home = dirs::home_dir().expect("the test environment has a home directory");
        assert_eq!(resolve(Some(OsString::from("~"))), Some(home.clone()));
        assert_eq!(
            resolve(Some(OsString::from("~/wing"))),
            Some(home.join("wing"))
        );

        // Everything else is a literal path: `~user` (no `getpwnam` in `std`),
        // a `~` that is not the leading component, and a non-UTF-8 value.
        assert_eq!(
            resolve(Some(OsString::from("~other/wing"))),
            Some(PathBuf::from("~other/wing"))
        );
        assert_eq!(
            resolve(Some(OsString::from("/srv/~/wing"))),
            Some(PathBuf::from("/srv/~/wing"))
        );
        assert_eq!(
            resolve(Some(OsString::from("wing/~"))),
            Some(PathBuf::from("wing/~"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_tilde_is_not_expanded_inside_raw_bytes() {
        // A leading `~` followed by bytes that are not UTF-8: the value is not
        // text, so it is passed through as it is (paths are bytes).
        assert_eq!(
            resolve(Some(raw(b"~\xff/wing"))),
            Some(PathBuf::from(raw(b"~\xff/wing")))
        );
        assert_eq!(
            resolve(Some(raw(b"/tmp/~\xff/wing"))),
            Some(PathBuf::from(raw(b"/tmp/~\xff/wing")))
        );
    }
}
