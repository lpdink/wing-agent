//! TUI 退出摘要——终端恢复后打印的会话用量总账。
//!
//! 只服务 **TUI 形态**（`cmd::run_tui` 在 `run_app` 返回、终端恢复之后调用）：
//! stdio / ACP 的 stdout 是协议通道，绝不走这里（形态由 `exit_summary_shape`
//! 守门测试钉住）。网关不可达时降级：只打印本地能证实的部分（墙钟 + 状态栏
//! 累计）并注明用量不可用。
//!
//! 渲染复用 CLI 表格引擎（`render::table::plain`，与 `wing ps` / `wing tools`
//! 同一份皮肤与宽度数学）——两处读起来是同一个产品。

#![allow(clippy::print_stdout)]

use ratatui::style::Style;

use crate::app::TuiExit;
use crate::cmd::common::TableOutput;
use crate::cmd::common::render_table;
use crate::config::ThemePalette;
use crate::render::table::ColumnKind;
use crate::render::table::plain::PlainCell;
use crate::render::table::plain::PlainColumn;
use crate::render::table::plain::PlainTable;
use wing_api_client::models::SessionUsage;

/// 打印退出摘要（stdout，终端已恢复；颜色循 CLI 惯例：TTY 才着色）。
pub fn print_exit_summary(exit: &TuiExit) {
    let out = TableOutput::detect();
    let palette = ThemePalette::from_config(&crate::config::AppConfig::load().colors);
    print!("{}", render_exit_summary(exit, &palette, &out));
}

/// 摘要的纯渲染（可测；打印与取数分离）。
pub fn render_exit_summary(exit: &TuiExit, palette: &ThemePalette, out: &TableOutput) -> String {
    let label_ink = Style::new().fg(palette.dim);
    let value_ink = Style::new().fg(palette.text);

    let mut rows: Vec<Vec<PlainCell>> = vec![row(
        "WALL CLOCK",
        fmt_duration(exit.wall),
        label_ink,
        value_ink,
    )];

    match &exit.usage {
        Some(usage) => {
            let work = usage.llm_time_ms + usage.tool_time_ms;
            rows.push(row(
                "LLM TIME",
                fmt_millis(usage.llm_time_ms),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "TOOL TIME",
                fmt_millis(usage.tool_time_ms),
                label_ink,
                value_ink,
            ));
            rows.push(row("WORK TIME", fmt_millis(work), label_ink, value_ink));
            rows.push(row(
                "USER MSGS",
                fmt_count(usage.user_messages),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "LLM CALLS",
                fmt_count(usage.llm_calls),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "TOOL CALLS",
                fmt_count(usage.tool_calls),
                label_ink,
                value_ink,
            ));
            rows.push(row("TURNS", fmt_count(usage.turns), label_ink, value_ink));
            rows.push(row("CACHE HIT", fmt_hit_rate(usage), label_ink, value_ink));
            rows.push(row(
                "INPUT TOKENS",
                fmt_tokens(usage.prompt_tokens),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "OUTPUT TOKENS",
                fmt_tokens(usage.completion_tokens),
                label_ink,
                value_ink,
            ));
        }
        None => {
            // 降级：网关不可达 / 连接已断——只报本地能证实的数字。
            rows.push(row(
                "CACHE HIT",
                hit_rate_from(exit.cached_tokens, exit.prompt_tokens),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "INPUT TOKENS",
                fmt_tokens(exit.prompt_tokens),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "OUTPUT TOKENS",
                fmt_tokens(exit.completion_tokens),
                label_ink,
                value_ink,
            ));
        }
    }

    let columns = vec![
        PlainColumn::new("SESSION", ColumnKind::Compact),
        PlainColumn::new("TOTAL", ColumnKind::Compact),
    ];
    let table = PlainTable { columns, rows };
    let mut rendered = String::new();
    rendered.push('\n');
    rendered.push_str(&format!(
        "session {} · {}\n",
        crate::cmd::common::single_line(&exit.session_id),
        crate::cmd::common::single_line(&exit.model)
    ));
    rendered.push_str(&render_table(&table, palette, out));
    if exit.usage.is_none() {
        rendered.push_str("usage unavailable: gateway unreachable (live counters only)\n");
    }
    rendered
}

fn row(label: &str, value: String, label_ink: Style, value_ink: Style) -> Vec<PlainCell> {
    vec![
        PlainCell::styled(label, label_ink),
        PlainCell::styled(value, value_ink),
    ]
}

/// 墙钟（会话级 `Duration`）：`3h 04m` / `12m 34s` / `8s`。
fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// 毫秒时长（账本字段）：与墙钟同形，但保留亚秒精度（`0.9s`）。
fn fmt_millis(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1_000 {
        // 有量但不足一秒时给两位小数——`0s` 会把真实的百毫秒调用抹平成「没跑过」。
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        fmt_duration(std::time::Duration::from_millis(ms as u64))
    }
}

/// 计数：原值（不缩写——用户会对账）。
fn fmt_count(n: i64) -> String {
    n.to_string()
}

/// token：与状态栏同一套紧凑记法（`1.2k` / `3.4M`）。
fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// 命中率 = cached / prompt（prompt 为 0 时是 `-`：没有可命中的输入）。
fn fmt_hit_rate(usage: &SessionUsage) -> String {
    hit_rate_from(usage.cached_tokens, usage.prompt_tokens)
}

fn hit_rate_from(cached: i64, prompt: i64) -> String {
    if prompt <= 0 {
        return "-".to_string();
    }
    format!("{:.1}%", cached.max(0) as f64 / prompt as f64 * 100.0)
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    /// 摘要里某个标签行的完整渲染（断言值落在正确的行上）。
    fn label_line(text: &str, label: &str) -> String {
        text.lines()
            .find(|line| line.contains(label))
            .unwrap_or_default()
            .to_string()
    }

    fn palette() -> ThemePalette {
        ThemePalette::from_config(&AppConfig::default().colors)
    }

    fn out() -> TableOutput {
        TableOutput {
            width: 100,
            color: false,
        }
    }

    fn exit(usage: Option<SessionUsage>) -> TuiExit {
        TuiExit {
            session_id: "20261019-210702-217d85f2".into(),
            model: "DeepSeek-Flash".into(),
            wall: std::time::Duration::from_secs(754),
            usage,
            prompt_tokens: 120_500,
            completion_tokens: 4_200,
            cached_tokens: 80_000,
        }
    }

    fn sample_usage() -> SessionUsage {
        SessionUsage {
            user_messages: 4,
            llm_calls: 9,
            tool_calls: 7,
            turns: 3,
            prompt_tokens: 120_500,
            completion_tokens: 4_200,
            cached_tokens: 80_000,
            llm_time_ms: 400_000,
            tool_time_ms: 82_000,
        }
    }

    #[test]
    fn renders_every_field_the_summary_owns() {
        let text = render_exit_summary(&exit(Some(sample_usage())), &palette(), &out());

        assert!(text.contains("20261019-210702-217d85f2"), "{text}");
        assert!(text.contains("DeepSeek-Flash"), "{text}");
        assert!(
            label_line(&text, "WALL CLOCK").contains("12m 34s"),
            "{text}"
        );
        assert!(label_line(&text, "LLM TIME").contains("6m 40s"), "{text}");
        assert!(label_line(&text, "TOOL TIME").contains("1m 22s"), "{text}");
        // 工作时长 = llm + tool（客户端推导，可断言）。
        assert!(label_line(&text, "WORK TIME").contains("8m 02s"), "{text}");
        assert!(label_line(&text, "USER MSGS").contains('4'), "{text}");
        assert!(label_line(&text, "LLM CALLS").contains('9'), "{text}");
        assert!(label_line(&text, "TOOL CALLS").contains('7'), "{text}");
        assert!(label_line(&text, "TURNS").contains('3'), "{text}");
        assert!(label_line(&text, "CACHE HIT").contains("66.4%"), "{text}");
        assert!(
            label_line(&text, "INPUT TOKENS").contains("120.5k"),
            "{text}"
        );
        assert!(
            label_line(&text, "OUTPUT TOKENS").contains("4.2k"),
            "{text}"
        );
        // 降级注记只在降级摘要里出现。
        assert!(!text.contains("usage unavailable"), "{text}");
    }

    #[test]
    fn degrades_to_live_counters_when_the_gateway_is_gone() {
        let text = render_exit_summary(&exit(None), &palette(), &out());

        assert!(
            label_line(&text, "WALL CLOCK").contains("12m 34s"),
            "{text}"
        );
        assert!(
            label_line(&text, "INPUT TOKENS").contains("120.5k"),
            "{text}"
        );
        assert!(label_line(&text, "CACHE HIT").contains("66.4%"), "{text}");
        assert!(text.contains("usage unavailable"), "{text}");
        // 只有在线的累计：没有服务端账本就不报时长 / 计数。
        assert!(!text.contains("LLM TIME"), "{text}");
        assert!(!text.contains("TURNS"), "{text}");
    }

    #[test]
    fn formats_durations_by_scale() {
        assert_eq!(fmt_duration(std::time::Duration::from_secs(8)), "8s");
        assert_eq!(fmt_duration(std::time::Duration::from_secs(754)), "12m 34s");
        assert_eq!(
            fmt_duration(std::time::Duration::from_secs(11_040)),
            "3h 04m"
        );
        assert_eq!(fmt_millis(0), "0.0s");
        assert_eq!(fmt_millis(940), "0.9s");
        assert_eq!(fmt_millis(82_000), "1m 22s");
    }

    #[test]
    fn hit_rate_is_derived_and_never_divides_by_zero() {
        assert_eq!(fmt_hit_rate(&sample_usage()), "66.4%");
        assert_eq!(hit_rate_from(0, 0), "-");
        assert_eq!(hit_rate_from(5, 10), "50.0%");
    }
}
