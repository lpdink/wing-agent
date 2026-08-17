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
    if resp.providers.is_empty() {
        println!("No models configured.");
        return;
    }
    for group in &resp.providers {
        println!("Provider: {}", group.provider);
        if group.models.is_empty() {
            println!("  (no models)");
        } else {
            for model in &group.models {
                println!("  {model}");
            }
        }
        println!();
    }
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
        } else if tool.description.len() > 40 {
            format!("{}...", &tool.description[..37])
        } else {
            tool.description.clone()
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
