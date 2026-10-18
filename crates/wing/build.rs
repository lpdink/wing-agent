//! build.rs — injects compile-time metadata (git commit, build time)
//! so the binary can report them via `--version` and the TUI header.
//!
//! Environment variables exposed:
//!   WING_COMMIT_HASH  — short git SHA (e.g. "a1b2c3d")
//!   WING_BUILD_TIME   — UTC timestamp  (e.g. "2025-01-15 10:30:00 UTC")
//!   WING_TARGET       — target triple   (e.g. "aarch64-apple-darwin")

use std::path::PathBuf;
use std::process::Command;

/// `git rev-parse <arg>` 的路径结果（相对输出按当前目录解析——build script 的 cwd
/// 就是包根，`--git-dir` 在主仓返回 `.git`）。git 不可用 / 不在仓库里返回 `None`。
fn git_path(arg: &str) -> Option<PathBuf> {
    let output = Command::new("git").args(["rev-parse", arg]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    let path = PathBuf::from(&text);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

/// 把 git 状态挂进 cargo 的变更检测。
///
/// 只监视**存在**的路径：`rerun-if-changed` 指向缺失路径时 cargo 每次都判 dirty
/// （`StaleItem(MissingFile)`）——而 worktree 的 `.git` 是文件不是目录，硬编码的
/// `../../.git/HEAD` 在那里永远不存在，于是每个 worktree 的**每次**构建都要重跑
/// 本脚本、重编 `wing` 及其 53 个测试二进制（本机实测 +10s/次）。
fn watch_git_state() {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for arg in ["--absolute-git-dir", "--git-common-dir"] {
        if let Some(dir) = git_path(arg)
            && !dirs.contains(&dir)
        {
            dirs.push(dir);
        }
    }
    // HEAD 是「当前在哪个提交」的唯一来源；refs / packed-refs（后者在 common dir，
    // worktree 自己的 git dir 只放 stash 一类的 per-worktree ref）跟着分支更新走。
    for dir in &dirs {
        for name in ["HEAD", "refs", "packed-refs"] {
            let path = dir.join(name);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

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
    println!("cargo:rerun-if-env-changed=WING_COMMIT_HASH");
    watch_git_state();

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
