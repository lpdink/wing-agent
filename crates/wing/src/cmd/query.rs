//! `wing models` / `wing tools` / `wing agents` — system query commands.
//!
//! These wrap the existing HTTP query endpoints and format the output
//! as tables (default) or JSON (`--json`).

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use ratatui::style::Style;
use wing_api_client::models::{AgentsResponse, ModelDetail, ModelsResponse, ToolsListResponse};

use crate::config::AppConfig;
use crate::config::ThemePalette;
use crate::render::table::ColumnKind;
use crate::render::table::plain::{PlainCell, PlainColumn, PlainTable};

use super::common;
use super::common::TableOutput;

// ============================================================
// wing models
// ============================================================

/// Entry point for `wing models`.
pub async fn run_models(json: bool) -> ExitCode {
    match fetch_models().await {
        Ok(resp) => {
            if json {
                common::print_json_compact(&resp);
            } else {
                print_models(&resp);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing models error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn fetch_models() -> Result<ModelsResponse> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    Ok(http.get_models().await?)
}

fn print_models(resp: &ModelsResponse) {
    print!("{}", format_models(resp));
}

/// Maximum characters of a model `description` shown in the table.
/// Same spirit as `print_tools`' 40-char cap; model blurbs run a bit longer.
const MODEL_DESCRIPTION_MAX_CHARS: usize = 60;

/// Render the `wing models` table.
///
/// Row shape: `  {id}` — the **reference word** is the first, authoritative column
/// (it is what every request sends) — plus `→ {name}` when the call name differs
/// from the id, `({display_name})` when a display label is declared, and the
/// truncated description.
fn format_models(resp: &ModelsResponse) -> String {
    if resp.providers.is_empty() {
        return "No models configured.\n".to_string();
    }
    let mut out = String::new();
    for group in &resp.providers {
        out.push_str(&format!("Provider: {}\n", group.provider));
        if group.models.is_empty() {
            out.push_str("  (no models)\n");
        } else {
            for detail in &group.models {
                out.push_str(&format_model_row(detail));
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

/// 一行模型：`  {id}` + `→ {name}`（id ≠ name 时）+ `({label})`（声明的展示名与
/// 调用名不同时）+ 截断的描述。展示层专用——请求一律用 id。
fn format_model_row(detail: &ModelDetail) -> String {
    let mut line = format!("  {}", detail.id);
    if detail.name != detail.id {
        line.push_str(&format!(" → {}", detail.name));
    }
    let label = detail.display_label();
    if label != detail.name {
        line.push_str(&format!(" ({label})"));
    }
    if let Some(desc) = detail
        .description
        .as_deref()
        .filter(|desc| !desc.trim().is_empty())
    {
        line.push_str("  ");
        line.push_str(&common::truncate_chars(desc, MODEL_DESCRIPTION_MAX_CHARS));
    }
    line
}

// ============================================================
// wing tools
// ============================================================

/// Entry point for `wing tools`.
pub async fn run_tools(json: bool) -> ExitCode {
    match fetch_tools().await {
        Ok(resp) => {
            if json {
                common::print_json_compact(&resp);
            } else {
                print_tools(&resp);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing tools error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn fetch_tools() -> Result<ToolsListResponse> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    Ok(http.list_tools().await?)
}

fn print_tools(resp: &ToolsListResponse) {
    if resp.tools.is_empty() {
        println!("No tools registered.");
        return;
    }
    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = TableOutput::detect();
    print!("{}", format_tools_table(resp, &palette, &out));
}

/// Render the `wing tools` table through the shared table engine — same skin
/// and width policy as `wing ps` and the TUI markdown tables.
fn format_tools_table(
    resp: &ToolsListResponse,
    palette: &ThemePalette,
    out: &TableOutput,
) -> String {
    let columns = vec![
        PlainColumn::new("LLM NAME", ColumnKind::Compact),
        PlainColumn::new("NAMESPACE", ColumnKind::Compact),
        PlainColumn::capped("DESCRIPTION", ColumnKind::Narrative, 72),
    ];
    let rows = resp
        .tools
        .iter()
        .map(|tool| {
            vec![
                PlainCell::styled(tool.llm_name.clone(), Style::new().fg(palette.text)),
                PlainCell::styled(tool.namespace.clone(), Style::new().fg(palette.tool_result)),
                PlainCell::styled(
                    description_cell(&tool.description),
                    Style::new().fg(palette.dim),
                ),
            ]
        })
        .collect();
    let table = PlainTable { columns, rows };
    common::render_table(&table, palette, out)
}

/// Tool descriptions are free text (often multi-line markdown blurbs): flatten
/// to one line and collapse whitespace; an empty description renders as `-`.
fn description_cell(desc: &str) -> String {
    let flat = common::single_line(desc);
    let collapsed = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        "-".to_string()
    } else {
        collapsed
    }
}

// ============================================================
// wing agents
// ============================================================

/// Entry point for `wing agents`.
pub async fn run_agents(json: bool) -> ExitCode {
    match fetch_agents().await {
        Ok(resp) => {
            if json {
                common::print_json_compact(&resp);
            } else {
                print_agents(&resp);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing agents error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn fetch_agents() -> Result<AgentsResponse> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    Ok(http.get_agents().await?)
}

fn print_agents(resp: &AgentsResponse) {
    if resp.agents.is_empty() {
        println!("No agent templates configured.");
        return;
    }
    for name in &resp.agents {
        let marker = if name == &resp.default_agent {
            " (default)"
        } else {
            ""
        };
        println!("  {name}{marker}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::{ModelDetail, ProviderModels, ToolInfo};

    fn group(provider: &str, details: Vec<ModelDetail>) -> ProviderModels {
        ProviderModels {
            provider: provider.into(),
            models: details,
        }
    }

    // ── wing tools（共享表格引擎）─────────────────────────────────

    fn tool(llm_name: &str, namespace: &str, description: &str) -> ToolInfo {
        ToolInfo {
            ref_field: format!("{namespace}.{llm_name}"),
            namespace: namespace.into(),
            name: llm_name.into(),
            llm_name: llm_name.into(),
            description: description.into(),
        }
    }

    fn tools_out() -> TableOutput {
        TableOutput {
            width: 100,
            color: false,
        }
    }

    #[test]
    fn tools_table_is_framed_and_aligned() {
        let resp = ToolsListResponse {
            tools: vec![
                tool("Bash", "core", "Execute a shell command"),
                tool(
                    "mcp__github__create_issue",
                    "mcp",
                    "Create a GitHub issue.\nSecond line of the blurb.",
                ),
            ],
        };
        let table = format_tools_table(&resp, &ThemePalette::default(), &tools_out());
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 7, "{table}");
        assert!(lines[0].starts_with('┏'), "{table}");
        assert!(lines[1].contains("LLM NAME") && lines[1].contains("DESCRIPTION"));
        // 每行等宽（显示列记账）。
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| unicode_width::UnicodeWidthStr::width(*l))
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert!(widths[0] <= 100, "{widths:?}");
        // 多行 description 被压平，不破格。
        assert!(lines[5].contains("Create a GitHub issue. Second line of the blurb."));
    }

    #[test]
    fn description_cell_flattens_and_falls_back() {
        assert_eq!(description_cell(""), "-");
        assert_eq!(description_cell("   "), "-");
        assert_eq!(description_cell("a\n\tb  c"), "a b c");
    }

    fn detail(
        id: &str,
        name: &str,
        display_name: Option<&str>,
        description: Option<&str>,
    ) -> ModelDetail {
        ModelDetail {
            id: id.into(),
            name: name.into(),
            display_name: display_name.map(str::to_string),
            description: description.map(str::to_string),
            capabilities: Default::default(),
        }
    }

    #[test]
    fn format_models_lists_ids_first() {
        // id == name（存量配置的常态）：第一列就是 id，不加冗余后缀。
        let resp = ModelsResponse {
            providers: vec![group(
                "qoder",
                vec![
                    detail("dfmodel", "dfmodel", None, None),
                    detail("gpt-x", "gpt-x", None, None),
                ],
            )],
        };
        assert_eq!(
            format_models(&resp),
            "Provider: qoder\n  dfmodel\n  gpt-x\n\n"
        );
    }

    #[test]
    fn format_models_adds_call_name_and_display_name_suffixes() {
        // id ≠ name → `→ name`；声明了展示名 → `(label)`；描述截断列在最后。
        let resp = ModelsResponse {
            providers: vec![group(
                "qoder",
                vec![
                    detail(
                        "ds-flash",
                        "dfmodel-2026",
                        Some("DeepSeek-Flash"),
                        Some("深度求索正式版模型"),
                    ),
                    detail("bare", "bare-upstream", None, None),
                ],
            )],
        };
        assert_eq!(
            format_models(&resp),
            "Provider: qoder\n  ds-flash → dfmodel-2026 (DeepSeek-Flash)  深度求索正式版模型\n  bare → bare-upstream\n\n"
        );
    }

    #[test]
    fn format_models_truncates_long_descriptions() {
        let long = "x".repeat(100);
        let resp = ModelsResponse {
            providers: vec![group(
                "p",
                vec![detail("m", "m", None, Some(long.as_str()))],
            )],
        };
        let expected_desc = "x".repeat(MODEL_DESCRIPTION_MAX_CHARS - 3) + "...";
        assert_eq!(
            format_models(&resp),
            format!("Provider: p\n  m  {expected_desc}\n\n")
        );
    }

    #[test]
    fn format_models_skips_redundant_or_empty_display_names() {
        // 展示名与调用名相同 / 为空串 → 不重复展示；无 description 不补尾巴。
        // id == name 且声明了展示名 → 只补 `(label)`。
        let resp = ModelsResponse {
            providers: vec![group(
                "p",
                vec![
                    detail("same", "same", Some("same"), None),
                    detail("empty", "empty", Some(""), None),
                    detail("labelled", "labelled", Some("Nice Label"), None),
                ],
            )],
        };
        assert_eq!(
            format_models(&resp),
            "Provider: p\n  same\n  empty\n  labelled (Nice Label)\n\n"
        );
    }

    #[test]
    fn format_models_skips_whitespace_only_description() {
        // 纯空白 description 不是有效值——补在行尾只会多出一串尾部空格。
        let resp = ModelsResponse {
            providers: vec![group(
                "p",
                vec![detail("blank", "blank", None, Some("   "))],
            )],
        };
        assert_eq!(format_models(&resp), "Provider: p\n  blank\n\n");
        // 带内容、两端有空白仍然展示（只做判定，不改写声明的值）。
        let resp = ModelsResponse {
            providers: vec![group(
                "p",
                vec![detail("padded", "padded", None, Some(" 说明 "))],
            )],
        };
        assert_eq!(format_models(&resp), "Provider: p\n  padded   说明 \n\n");
    }

    #[test]
    fn format_models_empty_and_providerless_responses() {
        assert_eq!(
            format_models(&ModelsResponse { providers: vec![] }),
            "No models configured.\n"
        );
        let empty_group = ModelsResponse {
            providers: vec![group("p", vec![])],
        };
        assert_eq!(
            format_models(&empty_group),
            "Provider: p\n  (no models)\n\n"
        );
    }
}
