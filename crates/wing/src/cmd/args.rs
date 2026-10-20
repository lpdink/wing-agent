//! Shared CLI argument structs for agent-launching subcommands.

use clap::Args;

/// Arguments of `wing run` (stdio mode carries its own `StdioArgs`).
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

    /// Agent to use for the session (name from `wing agents`; default: the
    /// gateway's default agent).
    ///
    /// Create-time only: with `-r` the resumed session keeps its agent (switch
    /// it with `wing update <sid> --agent <name>`).
    #[arg(long = "agent")]
    pub agent: Option<String>,

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

    /// Answer a pending Ask by its tool_call_id — routes the prompt to that
    /// ask's feedback waiter instead of the session inbox.
    ///
    /// Find the id with `wing asks <sid>` (the pending-ask query): a pending
    /// ask's tool call is not on the chain yet, so `wing tail` cannot see it.
    /// A stale id (no live waiter) falls back to a normal message.
    #[arg(long = "tool-call-id")]
    pub tool_call_id: Option<String>,

    /// Attach tags to the session (repeatable / comma-separated).
    ///
    /// Applied at creation; with -r the tags are added to the resumed
    /// session. Invalid tags fail the launch (nothing is sent).
    #[arg(long = "tag", value_delimiter = ',')]
    pub tag: Vec<String>,

    /// Do not register a wake-up on the new session (the implicit default is on).
    ///
    /// Without this flag `wing run` asks the gateway to wake MY session
    /// (`WING_SESSION_ID`, or `--notify-to`) when the spawned one finishes or
    /// asks a question. Inside an agent's shell that notification is the
    /// non-blocking replacement for `wing wait`; outside one there is no target
    /// and the registration is silently off either way.
    #[arg(long = "no-notify")]
    pub no_notify: bool,

    /// Deliver the wake-up to this session instead of `WING_SESSION_ID`.
    ///
    /// The gateway refuses a target equal to the newly created session (400);
    /// that error is reported verbatim but never fails the launch.
    #[arg(long = "notify-to", value_name = "SID")]
    pub notify_to: Option<String>,

    /// Which events wake the target: `done`, `ask`, or both (default: `done,ask`).
    #[arg(long = "notify-on", value_name = "EVENTS")]
    pub notify_on: Option<String>,

    /// Send this text verbatim instead of the built-in wake envelope.
    #[arg(long = "notify-prompt", value_name = "TEXT")]
    pub notify_prompt: Option<String>,
}
