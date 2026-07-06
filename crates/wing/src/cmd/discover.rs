//! Gateway executable discovery — search in known locations.

use std::path::{Path, PathBuf};

/// Find the `wing-gateway` executable.
///
/// Search order:
/// 1. `$WING_GATEWAY_CMD` environment variable
/// 2. `~/.wing/venv/bin/wing-gateway`
/// 3. `$PATH`
pub fn find_gateway_executable() -> anyhow::Result<PathBuf> {
    // 1. Environment override.
    if let Ok(cmd) = std::env::var("WING_GATEWAY_CMD") {
        let path = PathBuf::from(&cmd);
        if is_executable(&path) {
            return Ok(path);
        }
        anyhow::bail!("$WING_GATEWAY_CMD set to '{cmd}' but not an executable file");
    }

    // 2. Standard install path.
    let venv_path = super::state::wing_root()
        .join("venv")
        .join("bin")
        .join("wing-gateway");
    if is_executable(&venv_path) {
        return Ok(venv_path);
    }

    // 3. Search $PATH.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("wing-gateway");
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }

    anyhow::bail!(
        "wing-gateway not found. Searched:\n  \
         1. $WING_GATEWAY_CMD (not set)\n  \
         2. ~/.wing/venv/bin/wing-gateway (not found)\n  \
         3. $PATH (not found)\n\
         \n\
         Install the gateway: pip install wing-agent\n\
         Or set WING_GATEWAY_CMD to the executable path."
    );
}

/// Check if a path is a regular file with execute permission.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && path
                .metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
