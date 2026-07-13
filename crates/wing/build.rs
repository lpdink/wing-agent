//! build.rs — injects compile-time metadata (git commit, build time)
//! so the binary can report them via `--version` and the TUI header.
//!
//! Environment variables exposed:
//!   WING_COMMIT_HASH  — short git SHA (e.g. "a1b2c3d")
//!   WING_BUILD_TIME   — UTC timestamp  (e.g. "2025-01-15 10:30:00 UTC")
//!   WING_TARGET       — target triple   (e.g. "aarch64-apple-darwin")

use std::process::Command;

fn main() {
    // ── Git commit hash ───────────────────────────────────────────
    // CI passes the full SHA via env var (git may be unavailable in
    // Docker containers such as manylinux_2_28).  Fall back to running
    // `git rev-parse` for local builds.
    let commit = std::env::var("WING_COMMIT_HASH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "unknown".to_string())
        });

    // Truncate to 7 chars to match `git rev-parse --short` behaviour.
    let commit = if commit.len() > 7 {
        commit[..7].to_string()
    } else {
        commit
    };

    println!("cargo:rustc-env=WING_COMMIT_HASH={commit}");

    // Re-run when HEAD changes (checkout, commit, rebase, etc.).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");

    // ── Build time (UTC) ──────────────────────────────────────────
    let build_time = Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M:%S UTC"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=WING_BUILD_TIME={build_time}");

    // ── Target triple (for informational purposes) ────────────────
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=WING_TARGET={target}");
}
