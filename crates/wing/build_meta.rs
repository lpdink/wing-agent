//! 构建期 commit 的解析：`build.rs` 编进构建脚本，lib 侧只在测试里编译
//! （`src/lib.rs` 的 `#[path]` 引入）—— 三档顺序与取舍因此能被 `cargo test` 覆盖，
//! 而构建脚本本身不在测试与覆盖率面内。
//!
//! 三档来源按**显式程度**排序：CI 注入的 `WING_COMMIT_HASH` → 有 git 的构建
//! （`git rev-parse`）→ `build_commit` 文件。文件档只为 manylinux 存在：maturin 在
//! Docker 里构建，宿主 step 的 env 不进容器、镜像也不带 git，另两条路会同时断。
//! 它排在 git **之后**：一份陈旧的、被 gitignore 藏起来的输入绝不能盖住仓库真实的
//! HEAD（静默的假 hash 比 `unknown` 危险）。细节见 `docs/dev/architecture.md`
//! 「构建信息注入」。

use std::path::Path;

/// 构建输入文件名（`crates/wing/build_commit`，gitignore；发布 workflow 在构建前写入）。
pub(crate) const COMMIT_FILE: &str = "build_commit";

/// 三档全断时的哨兵。与 `cmd::status::NO_HASH` 同值但不同层：本模块也编进构建脚本
/// （不链接本 crate），共享不了常量。
pub(crate) const NO_COMMIT: &str = "unknown";

/// 一行 commit 的读取（缺失、不可读、空白都算没有）。
pub(crate) fn read_commit_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    non_empty(&text)
}

/// 三档来源的决策：`env` → `git` → `file`，全空时 [`NO_COMMIT`]；结果截到 7 位。
pub(crate) fn resolve(env: Option<&str>, git: Option<&str>, file: Option<&str>) -> String {
    [env, git, file]
        .into_iter()
        .flatten()
        .find_map(non_empty)
        .map(|commit| commit.chars().take(7).collect())
        .unwrap_or_else(|| NO_COMMIT.to_string())
}

fn non_empty(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_wins_over_git_and_file() {
        assert_eq!(
            resolve(Some("envc0de"), Some("git0c0d"), Some("file000")),
            "envc0de"
        );
    }

    #[test]
    fn a_stale_file_never_shadows_the_repository_head() {
        // 文件被 gitignore 藏着：若它排在 git 之前，一次误写的残留会让二进制永久
        // 报那个 hash，且 `git status` 看不见。
        assert_eq!(resolve(None, Some("git0c0d"), Some("file000")), "git0c0d");
    }

    #[test]
    fn the_file_is_the_last_resort_for_builds_without_git() {
        assert_eq!(resolve(None, None, Some("file000")), "file000");
    }

    #[test]
    fn blank_sources_are_skipped() {
        assert_eq!(resolve(Some("   "), None, Some(" file01 ")), "file01");
        assert_eq!(resolve(Some(""), Some("\t"), Some("\n")), NO_COMMIT);
    }

    #[test]
    fn commits_are_truncated_to_seven_chars() {
        assert_eq!(resolve(Some("0123456789abcdef"), None, None), "0123456");
        assert_eq!(resolve(Some("abc"), None, None), "abc");
    }

    #[test]
    fn non_ascii_input_does_not_split_a_character() {
        assert_eq!(resolve(Some("发版构建abcdef"), None, None), "发版构建abc");
    }

    #[test]
    fn nothing_resolved_reports_unknown() {
        assert_eq!(resolve(None, None, None), NO_COMMIT);
    }

    #[test]
    fn the_commit_file_is_read_trimmed_and_blank_means_absent() {
        let dir = std::env::temp_dir().join(format!("wing-build-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(COMMIT_FILE);

        std::fs::write(&path, "a1b2c3d4e5f6\n").unwrap();
        assert_eq!(read_commit_file(&path).as_deref(), Some("a1b2c3d4e5f6"));

        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(read_commit_file(&path), None);

        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_commit_file(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
