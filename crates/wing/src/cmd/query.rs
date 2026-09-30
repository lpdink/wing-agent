//! `wing models` / `wing tools` / `wing agents` — system query commands.
//!
//! These wrap the existing HTTP query endpoints and format the output
//! as tables (default) or JSON (`--json`).

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use wing_api_client::models::{AgentsResponse, ModelsResponse, ToolsListResponse};

use super::common;

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
/// Row shape: `  {name}` + ` ({display_name})` when the gateway declared a
/// display name distinct from the call name + the truncated description.
/// Display only — the call name stays the first, authoritative column.
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
            for model in &group.models {
                let label = group.label_for(model);
                let mut line = if label == model {
                    format!("  {model}")
                } else {
                    format!("  {model} ({label})")
                };
                if let Some(desc) = group
                    .detail_for(model)
                    .and_then(|detail| detail.description.as_deref())
                    .filter(|desc| !desc.is_empty())
                {
                    line.push_str("  ");
                    line.push_str(&common::truncate_chars(desc, MODEL_DESCRIPTION_MAX_CHARS));
                }
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
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

    let name_w = 25;
    let ns_w = 15;

    println!("{:<name_w$} {:<ns_w$} DESCRIPTION", "LLM NAME", "NAMESPACE");
    println!("{}", "-".repeat(name_w + ns_w + 40));

    for tool in &resp.tools {
        let desc: String = if tool.description.is_empty() {
            "-".to_string()
        } else {
            common::truncate_chars(&tool.description, 40)
        };
        println!(
            "{:<name_w$} {:<ns_w$} {desc}",
            tool.llm_name, tool.namespace
        );
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
    use wing_api_client::models::{ModelDetail, ProviderModels};

    fn group(provider: &str, models: &[&str], details: Vec<ModelDetail>) -> ProviderModels {
        ProviderModels {
            provider: provider.into(),
            models: models.iter().map(|m| m.to_string()).collect(),
            model_details: details,
        }
    }

    fn detail(name: &str, display_name: Option<&str>, description: Option<&str>) -> ModelDetail {
        ModelDetail {
            name: name.into(),
            display_name: display_name.map(str::to_string),
            description: description.map(str::to_string),
            capabilities: Default::default(),
        }
    }

    #[test]
    fn format_models_legacy_response_prints_call_names_only() {
        let resp = ModelsResponse {
            providers: vec![group("qoder", &["dfmodel", "gpt-x"], vec![])],
        };
        assert_eq!(
            format_models(&resp),
            "Provider: qoder\n  dfmodel\n  gpt-x\n\n"
        );
    }

    #[test]
    fn format_models_adds_display_name_and_description() {
        let resp = ModelsResponse {
            providers: vec![group(
                "qoder",
                &["dfmodel", "bare"],
                vec![detail(
                    "dfmodel",
                    Some("DeepSeek-Flash"),
                    Some("深度求索正式版模型"),
                )],
            )],
        };
        assert_eq!(
            format_models(&resp),
            "Provider: qoder\n  dfmodel (DeepSeek-Flash)  深度求索正式版模型\n  bare\n\n"
        );
    }

    #[test]
    fn format_models_truncates_long_descriptions() {
        let long = "x".repeat(100);
        let resp = ModelsResponse {
            providers: vec![group(
                "p",
                &["m"],
                vec![detail("m", None, Some(long.as_str()))],
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
        // display_name 与调用名相同 / 为空串 → 不重复展示；无 description 不补尾巴。
        let resp = ModelsResponse {
            providers: vec![group(
                "p",
                &["same", "empty"],
                vec![
                    detail("same", Some("same"), None),
                    detail("empty", Some(""), None),
                ],
            )],
        };
        assert_eq!(format_models(&resp), "Provider: p\n  same\n  empty\n\n");
    }

    #[test]
    fn format_models_empty_and_providerless_responses() {
        assert_eq!(
            format_models(&ModelsResponse { providers: vec![] }),
            "No models configured.\n"
        );
        let empty_group = ModelsResponse {
            providers: vec![group("p", &[], vec![])],
        };
        assert_eq!(
            format_models(&empty_group),
            "Provider: p\n  (no models)\n\n"
        );
    }
}
