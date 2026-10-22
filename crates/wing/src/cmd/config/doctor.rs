//! `wing config doctor`：只打 `GET /api/settings/status`（最便宜的预检），
//! 可用 = 0、有问题 = 1。

use serde::Serialize;
use wing_api_client::models::{SettingProblem, SettingsStatusResponse};

use super::Outcome;
use super::SettingsApi;
use super::api_failure;
use super::catalog::sorted_problems;
use super::config_path;
use super::render::json_string;

/// `wing config doctor`：只打 `status`（最便宜的预检），可用 = 0，有问题 = 1。
pub(super) async fn execute_doctor<B: SettingsApi>(
    backend: &B,
    json: bool,
    http_base: &str,
) -> Outcome {
    let status = match backend.status().await {
        Ok(status) => status,
        Err(e) => {
            let (code, message) = api_failure(&e, http_base, "读取配置状态失败");
            return Outcome::failure(code, format!("{message}\n"));
        }
    };

    let problems = sorted_problems(&status.problems);
    let path = config_path().to_string_lossy().to_string();
    let stdout = if json {
        let output = DoctorOutput {
            valid: status.valid,
            setup_mode: status.setup_mode,
            config_path: path,
            fingerprint: status.fingerprint.clone(),
            problems: problems.into_iter().cloned().collect(),
        };
        format!("{}\n", json_string(&output))
    } else {
        render_doctor(&status, &problems, &path)
    };

    if status.valid {
        Outcome::ok(stdout)
    } else {
        Outcome::refused(stdout)
    }
}

/// `--json` 输出形状：`wing config doctor`。
#[derive(Debug, Serialize)]
struct DoctorOutput {
    valid: bool,
    setup_mode: bool,
    config_path: String,
    fingerprint: Option<String>,
    problems: Vec<SettingProblem>,
}

/// doctor 的人类可读输出（纯函数，便于断言）。
fn render_doctor(
    status: &SettingsStatusResponse,
    problems: &[&SettingProblem],
    config_path: &str,
) -> String {
    if status.valid {
        return format!("✓ 配置可用（{config_path}）\n");
    }
    let mut out = String::new();
    let mode = if status.setup_mode {
        "· 网关处于修复模式"
    } else {
        ""
    };
    out.push_str(&format!("✗ 配置不可用（{config_path}）{mode}\n"));
    for problem in problems {
        let at = problem.path.as_deref().unwrap_or("(文档)");
        out.push_str(&format!("  {at}: {}\n", problem.message));
        if let Some(hint) = problem.hint.as_deref().filter(|h| !h.trim().is_empty()) {
            out.push_str(&format!("      ↳ {hint}\n"));
        }
    }
    if problems.is_empty() {
        // 后端不该发出"不可用但零问题"的组合；真出现就如实说明，而不是打印「共 0 个问题」。
        out.push_str(
            "网关报告配置不可用，但没有给出具体问题；查看网关日志或运行 wing config list\n",
        );
        return out;
    }
    out.push_str(&format!(
        "共 {} 个问题 · 运行 wing 打开设置面板，或 wing config set <path> <value>\n",
        problems.len()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::config::testkit::*;
    use wing_api_client::models::SettingsStatusResponse;

    #[test]
    fn sorted_problems_orders_by_severity_then_path_and_unknown_last() {
        let problems = vec![
            problem(Some("b"), "unknown_key", "unknown", None),
            problem(Some("z"), "invalid_value", "bad", None),
            problem(None, "missing_required", "gone", None),
            problem(Some("a"), "invalid_value", "bad", None),
            problem(Some("c"), "weird_future_kind", "?", None),
        ];
        let sorted: Vec<&str> = sorted_problems(&problems)
            .into_iter()
            .map(|p| p.path.as_deref().unwrap_or("(doc)"))
            .collect();
        assert_eq!(sorted, vec!["(doc)", "a", "z", "b", "c"]);
    }
    #[test]
    fn render_doctor_renders_ok_and_problem_lines() {
        let ok = SettingsStatusResponse {
            valid: true,
            setup_mode: false,
            problems: Vec::new(),
            fingerprint: Some("fp-1".to_string()),
        };
        assert_eq!(
            render_doctor(&ok, &[], "/tmp/config.yaml"),
            "✓ 配置可用（/tmp/config.yaml）\n"
        );

        let status = SettingsStatusResponse {
            valid: false,
            setup_mode: true,
            problems: vec![
                problem(
                    Some("providers"),
                    "empty_list",
                    "providers 不得为空",
                    Some("新增一个 provider"),
                ),
                problem(None, "invalid_value", "文档无法解析", None),
            ],
            fingerprint: None,
        };
        let problems = sorted_problems(&status.problems);
        let rendered = render_doctor(&status, &problems, "/tmp/config.yaml");
        assert!(
            rendered.starts_with("✗ 配置不可用（/tmp/config.yaml）· 网关处于修复模式"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  providers: providers 不得为空"),
            "{rendered}"
        );
        assert!(rendered.contains("      ↳ 新增一个 provider"), "{rendered}");
        assert!(rendered.contains("  (文档): 文档无法解析"), "{rendered}");
        assert!(rendered.contains("共 2 个问题"), "{rendered}");
        assert!(
            rendered.contains("wing config set <path> <value>"),
            "{rendered}"
        );
    }
    /// `valid=false` 且零问题的边界组合（后端不该发，但文案不能是「共 0 个问题」）。
    #[test]
    fn render_doctor_handles_invalid_without_problems() {
        let status = SettingsStatusResponse {
            valid: false,
            setup_mode: false,
            problems: Vec::new(),
            fingerprint: None,
        };
        let text = render_doctor(&status, &[], "/tmp/config.yaml");
        assert!(text.starts_with("✗ 配置不可用"), "{text}");
        assert!(text.contains("没有给出具体问题"), "{text}");
        assert!(!text.contains("共 0 个问题"), "{text}");
    }
}
