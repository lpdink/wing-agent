//! Shared CLI argument structs for agent-launching subcommands.

use clap::Args;

/// Arguments shared by `wing run` and stdio mode (`wing -p`).
///
/// All fields optional except where noted. When a field is `None`,
/// the agent template's default value is used.
#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Prompt text — the task to execute.
    #[arg(short = 'p', long = "prompt")]
    pub prompt: String,

    /// Override model id (references `providers[].models` in the gateway config).
    #[arg(short = 'm', long = "model")]
    pub model: Option<String>,

    /// Resume an existing session by ID.
    #[arg(short = 'r', long = "resume")]
    pub resume: Option<String>,

    /// Replace system prompt.
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    /// Append to system prompt.
    #[arg(long = "append-system-prompt")]
    pub append_system_prompt: Option<String>,

    /// Maximum agent loop turns.
    #[arg(long = "max-turns")]
    pub max_turns: Option<u32>,

    /// Reasoning effort level: low|medium|high|xhigh|max.
    #[arg(long = "effort")]
    pub effort: Option<String>,

    /// Override tools (comma-separated). If not set, uses template defaults.
    #[arg(long = "tools")]
    pub tools: Option<String>,

    /// Attach tags to the session (repeatable / comma-separated).
    ///
    /// Applied at creation; with -r the tags are added to the resumed
    /// session. Invalid tags fail the launch (nothing is sent).
    #[arg(long = "tag", value_delimiter = ',')]
    pub tag: Vec<String>,
}
