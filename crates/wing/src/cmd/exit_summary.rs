//! TUI 退出摘要——终端恢复后打印的会话用量总账。
//!
//! 只服务 **TUI 形态**（`cmd::run_tui` 在 `run_app` 返回、终端恢复之后调用）：
//! stdio / ACP 的 stdout 是协议通道，绝不走这里（形态由 `exit_summary_shape`
//! 守门测试钉住）。账本读数失败时降级：只打印本地能证实的部分（墙钟 +
//! 状态栏累计）并注明原因。
//!
//! 渲染复用 CLI 表格引擎（`render::table::plain`，与 `wing ps` / `wing tools`
//! 同一份皮肤与宽度数学）——两处读起来是同一个产品。

#![allow(clippy::print_stdout)]

use std::time::Duration;

use ratatui::style::Style;

use crate::app::TuiExit;
use crate::cmd::common::TableOutput;
use crate::cmd::common::render_table;
use crate::config::ThemePalette;
use crate::render::table::ColumnKind;
use crate::render::table::plain::PlainCell;
use crate::render::table::plain::PlainColumn;
use crate::render::table::plain::PlainTable;
use crate::util::format;
use wing_api_client::GatewayClient;
use wing_api_client::models::SessionUsage;

/// 退出摘要的账本读数预算：超时即有界降级——摘要在终端恢复**之后**取数，
/// 一个不响应的网关不该让用户对着黑屏等。
const LEDGER_TIMEOUT: Duration = Duration::from_secs(2);

/// 退出时向网关要一次用量账本的结果。
///
/// 三态分开：`Reported` 才是完整摘要；`Unreported`（网关可达但没有这个
/// 字段——旧网关）与 `Unreachable`（请求失败 / 超时 / 会话已不在）都降级，
/// 但注记不同——把三种成因写成同一句「网关不可达」是误导。
pub enum Ledger {
    Reported(SessionUsage),
    Unreported,
    Unreachable,
}

/// 取一次退出用的账本快照（有界等待；失败分类不抛）。
pub async fn read_ledger(http_base: &str, api_key: Option<&str>, session_id: &str) -> Ledger {
    let Ok(client) = GatewayClient::new(http_base, api_key) else {
        return Ledger::Unreachable;
    };
    match tokio::time::timeout(LEDGER_TIMEOUT, client.get_session_info(session_id)).await {
        Ok(Ok(info)) => match info.usage {
            Some(usage) => Ledger::Reported(usage),
            None => Ledger::Unreported,
        },
        Ok(Err(e)) => {
            tracing::debug!("exit summary: session info unavailable: {e}");
            Ledger::Unreachable
        }
        Err(_) => {
            tracing::debug!("exit summary: session info timed out");
            Ledger::Unreachable
        }
    }
}

/// 打印退出摘要（stdout，终端已恢复；颜色循 CLI 惯例：TTY 才着色）。
///
/// 取数在终端恢复**之后**（由 TUI 入口调用）——备用屏已经交还，一个挂着不
/// 回话的网关只让摘要晚 2 秒、不冻住退出。
pub async fn print_exit_summary(http_base: &str, api_key: Option<&str>, exit: &TuiExit) {
    let ledger = read_ledger(http_base, api_key, &exit.session_id).await;
    let out = TableOutput::detect();
    // 同进程里探测答案还在：TUI 刚退出，摘要不该换回另一套配色（亮底终端上
    // `Wing.text` 只有 1.21:1，值列会看不见）。
    let palette = ThemePalette::from_config_with_appearance(
        &crate::config::AppConfig::load().colors,
        crate::tui::detected_appearance(),
    );
    print!("{}", render_exit_summary(exit, ledger, &palette, &out));
}

/// 摘要的纯渲染（可测；取数与打印分离）。
pub fn render_exit_summary(
    exit: &TuiExit,
    ledger: Ledger,
    palette: &ThemePalette,
    out: &TableOutput,
) -> String {
    let label_ink = Style::new().fg(palette.dim);
    let value_ink = Style::new().fg(palette.text);

    let mut rows: Vec<Vec<PlainCell>> = vec![row(
        "WALL CLOCK",
        fmt_duration(exit.wall),
        label_ink,
        value_ink,
    )];

    let note = match ledger {
        Ledger::Reported(usage) => {
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
                usage.user_messages.to_string(),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "LLM CALLS",
                usage.llm_calls.to_string(),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "TOOL CALLS",
                usage.tool_calls.to_string(),
                label_ink,
                value_ink,
            ));
            rows.push(row("TURNS", usage.turns.to_string(), label_ink, value_ink));
            rows.push(row("CACHE HIT", fmt_hit_rate(&usage), label_ink, value_ink));
            rows.push(row(
                "INPUT TOKENS",
                format::tokens(usage.prompt_tokens),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "OUTPUT TOKENS",
                format::tokens(usage.completion_tokens),
                label_ink,
                value_ink,
            ));
            None
        }
        other => {
            // 降级：只报本地能证实的数字（状态栏累计 = 快照基线 + live 增量）。
            rows.push(row(
                "CACHE HIT",
                fmt_hit_rate_from(exit.cached_tokens, exit.prompt_tokens),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "INPUT TOKENS",
                format::tokens(exit.prompt_tokens),
                label_ink,
                value_ink,
            ));
            rows.push(row(
                "OUTPUT TOKENS",
                format::tokens(exit.completion_tokens),
                label_ink,
                value_ink,
            ));
            Some(match other {
                Ledger::Unreported => {
                    "usage unavailable: the gateway reported none (live counters only)"
                }
                _ => "usage unavailable: gateway unreachable (live counters only)",
            })
        }
    };

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
    // 两种时间轴同框：墙钟是本次运行，其余行是会话累计——resume 一个老会话
    // 只跑 10 秒就退出时，不解释会看着像矛盾。
    rendered.push_str("wall clock covers this run; the other rows are session totals\n");
    if let Some(note) = note {
        rendered.push_str(note);
        rendered.push('\n');
    }
    rendered
}

fn row(label: &str, value: String, label_ink: Style, value_ink: Style) -> Vec<PlainCell> {
    vec![
        PlainCell::styled(label, label_ink),
        PlainCell::styled(value, value_ink),
    ]
}

/// 墙钟（本次 TUI 运行）：`3h 04m` / `12m 34s` / `8s`。
fn fmt_duration(d: Duration) -> String {
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

/// 毫秒时长（账本字段）：与墙钟同形，但保留亚秒精度（`0.9s`）——`0s` 会把
/// 真实的百毫秒调用抹平成「没跑过」。
fn fmt_millis(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        fmt_duration(Duration::from_millis(ms as u64))
    }
}

/// 命中率 = cached / prompt（一位小数；无输入 = `-`）。
fn fmt_hit_rate(usage: &SessionUsage) -> String {
    fmt_hit_rate_from(usage.cached_tokens, usage.prompt_tokens)
}

fn fmt_hit_rate_from(cached: i64, prompt: i64) -> String {
    match format::hit_rate(cached, prompt) {
        Some(rate) => format!("{rate:.1}%"),
        None => "-".to_string(),
    }
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

    fn exit() -> TuiExit {
        TuiExit {
            session_id: "20261019-210702-217d85f2".into(),
            model: "DeepSeek-Flash".into(),
            wall: Duration::from_secs(754),
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
        let text = render_exit_summary(
            &exit(),
            Ledger::Reported(sample_usage()),
            &palette(),
            &out(),
        );

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
        // 时间轴说明常在；降级注记只在降级时出现。
        assert!(text.contains("wall clock covers this run"), "{text}");
        assert!(!text.contains("usage unavailable"), "{text}");
    }

    #[test]
    fn degrades_to_live_counters_and_names_the_reason() {
        for (ledger, expected) in [
            (Ledger::Unreachable, "gateway unreachable"),
            (Ledger::Unreported, "reported none"),
        ] {
            let text = render_exit_summary(&exit(), ledger, &palette(), &out());

            assert!(
                label_line(&text, "WALL CLOCK").contains("12m 34s"),
                "{text}"
            );
            assert!(
                label_line(&text, "INPUT TOKENS").contains("120.5k"),
                "{text}"
            );
            assert!(label_line(&text, "CACHE HIT").contains("66.4%"), "{text}");
            assert!(text.contains(expected), "{text}");
            // 只有在线的累计：没有服务端账本就不报时长 / 计数。
            assert!(!text.contains("LLM TIME"), "{text}");
            assert!(!text.contains("TURNS"), "{text}");
        }
    }

    #[test]
    fn formats_durations_by_scale() {
        assert_eq!(fmt_duration(Duration::from_secs(8)), "8s");
        assert_eq!(fmt_duration(Duration::from_secs(754)), "12m 34s");
        assert_eq!(fmt_duration(Duration::from_secs(11_040)), "3h 04m");
        assert_eq!(fmt_millis(0), "0.0s");
        assert_eq!(fmt_millis(940), "0.9s");
        assert_eq!(fmt_millis(82_000), "1m 22s");
    }

    #[test]
    fn hit_rate_is_derived_and_never_divides_by_zero() {
        assert_eq!(fmt_hit_rate(&sample_usage()), "66.4%");
        assert_eq!(fmt_hit_rate_from(0, 0), "-");
        assert_eq!(fmt_hit_rate_from(5, 10), "50.0%");
    }

    /// 最小 HTTP 桩：对每个请求回一段固定 JSON。返回 base URL。
    async fn spawn_stub(body: &'static str) -> String {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn read_ledger_reports_the_usage_the_gateway_sent() {
        let base = spawn_stub(
            r#"{"model":"m","api_url":"http://x","tools":[],"total_tokens":0,
                "context_window_tokens":0,"thinking":false,"yolo":false,
                "context_stats":{"message_count":0,"total_tokens":0},
                "usage":{"prompt_tokens":7,"turns":2,"llm_time_ms":5}}"#,
        )
        .await;

        match read_ledger(&base, None, "s1").await {
            Ledger::Reported(usage) => {
                assert_eq!(usage.prompt_tokens, 7);
                assert_eq!(usage.turns, 2);
                assert_eq!(usage.llm_time_ms, 5);
            }
            _ => panic!("gateway shipped usage — expected Reported"),
        }
    }

    #[tokio::test]
    async fn read_ledger_names_the_reason_it_cannot_report() {
        // 旧网关：接口 200 但没有 usage 字段。
        let base = spawn_stub(
            r#"{"model":"m","api_url":"http://x","tools":[],"total_tokens":0,
                "context_window_tokens":0,"thinking":false,"yolo":false,
                "context_stats":{"message_count":0,"total_tokens":0}}"#,
        )
        .await;
        assert!(matches!(
            read_ledger(&base, None, "s1").await,
            Ledger::Unreported
        ));

        // 端口上没有监听者 / base URL 非法：都归 Unreachable，绝不 panic。
        assert!(matches!(
            read_ledger("http://127.0.0.1:1", None, "s1").await,
            Ledger::Unreachable
        ));
        assert!(matches!(
            read_ledger("not a url", None, "s1").await,
            Ledger::Unreachable
        ));
    }

    /// 打印路径本身（stdout 由测试框架捕获）——终端恢复后的唯一出口。
    #[tokio::test]
    async fn print_exit_summary_renders_the_degraded_variant_when_unreachable() {
        print_exit_summary("http://127.0.0.1:1", None, &exit()).await;
    }
}
