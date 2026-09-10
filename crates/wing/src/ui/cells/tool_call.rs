//! ToolCallBlock — tool invocation with result display.
//!
//! Uses enum dispatch (`ToolRenderer`) for tool-specific header and result
//! rendering. Known tools get tailored display; unknown tools fall back to
//! generic rendering.
//!
//! Header examples:
//!   ⦁ Bash(ls -la) 3s/30s
//!   ⦁ Read(wing/src/main.rs)
//!   ⦁ Glob(src/**/*.ts, **/*.py)
//!   ⦁ AskUserQuestion
//!
//! Result strategies:
//!   Hidden    — success result suppressed (Read, Write, Edit, Glob, Grep)
//!   Full      — result shown in full (AskUserQuestion)
//!   Truncated — head/tail with ellipsis (Bash, fallback)
//!   Failed    — always truncated to 50 chars

use std::time::Instant;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::app::constants;
use crate::config::ThemePalette;
use crate::render::markdown::types::MarkdownLine;
use crate::render::markdown::types::SegmentKind;
use crate::render::syntax;
use crate::ui::cells::todo_msg::TodoMessage;
use crate::ui::cells::todo_msg::render_todo_items;

/// Maximum characters for failed result display.
const FAILED_RESULT_MAX_CHARS: usize = 50;
/// Number of trailing path segments to keep.
const PATH_SEGMENT_COUNT: usize = 6;
/// Maximum lines for Edit streaming new_string preview.
const EDIT_STREAM_MAX_NEW_LINES: usize = 20;
/// Maximum lines for Edit streaming old_string before collapsing.
const EDIT_STREAM_MAX_OLD_LINES: usize = 8;
/// Lines to keep at head/tail when collapsing old_string.
const EDIT_STREAM_OLD_COLLAPSE_KEEP: usize = 3;

// ── ToolStatus ──────────────────────────────────────────────────

/// Tool call status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Args still streaming from LLM (◌ hollow bullet, dim).
    Streaming,
    /// Args complete, tool executing or about to execute (⦁ solid, warning).
    Pending,
    Success,
    Failed,
}

impl ToolStatus {
    fn color(&self, palette: &ThemePalette) -> Color {
        match self {
            Self::Streaming => palette.dim,
            Self::Pending => palette.warning,
            Self::Success => palette.success,
            Self::Failed => palette.danger,
        }
    }

    fn bullet(&self) -> &'static str {
        match self {
            Self::Streaming => "◌",
            _ => "⦁",
        }
    }
}

// ── WriteHighlightCache ────────────────────────────────────────

/// Incremental syntax highlight cache for Write tool streaming.
///
/// Follows PI's `updateWriteHighlightCacheIncremental` strategy:
/// - Maintains raw_content + highlighted_lines cache
/// - On delta: only highlights new/changed lines (last line + appended lines)
/// - Uses single-line highlighting (no multi-line context) during streaming
#[derive(Debug, Clone)]
pub struct WriteHighlightCache {
    /// Raw content string seen so far.
    raw_content: String,
    /// Detected language name (syntect syntax name).
    lang: String,
    /// Highlighted output lines (one per source line).
    highlighted_lines: Vec<MarkdownLine>,
    /// Raw text of the last line (for O(1) incremental append).
    last_line_raw: String,
}

/// Maximum lines to display for streaming Write content.
const WRITE_STREAM_MAX_LINES: usize = 20;

/// Strip trailing `\r` for CRLF consistency.
fn strip_cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

impl WriteHighlightCache {
    /// Create or update the cache incrementally.
    ///
    /// Falls back to plain-text rendering when path has no detectable
    /// extension (LLMs may emit `content` before `path` in args JSON).
    /// When the real path arrives later, the lang mismatch triggers a
    /// full rebuild with proper syntax highlighting.
    pub fn update(
        cache: Option<WriteHighlightCache>,
        path: &str,
        content: &str,
    ) -> Option<WriteHighlightCache> {
        // Empty string → highlight_single_line returns None → plain_line fallback.
        let lang = syntax::detect_syntax_from_path(path).unwrap_or_default();

        let mut cache = match cache {
            Some(c) if c.lang == lang && content.starts_with(&c.raw_content) => c,
            // Full rebuild: new cache or content doesn't match prefix.
            // Use split('\n') (not .lines()) to preserve trailing empty line
            // from content ending with '\n' — keeps incremental path consistent.
            _ => {
                let raw_lines: Vec<&str> = content.split('\n').collect();
                let highlighted: Vec<MarkdownLine> = raw_lines
                    .iter()
                    .map(|line| {
                        let clean = strip_cr(line);
                        syntax::highlight_single_line(clean, &lang)
                            .unwrap_or_else(|| plain_line(clean))
                    })
                    .collect();
                let last_line_raw = raw_lines
                    .last()
                    .map(|s| strip_cr(s).to_string())
                    .unwrap_or_default();
                return Some(WriteHighlightCache {
                    raw_content: content.to_string(),
                    lang,
                    highlighted_lines: highlighted,
                    last_line_raw,
                });
            }
        };

        // Incremental: content is a prefix extension of cached content
        if content.len() == cache.raw_content.len() {
            return Some(cache); // No change
        }

        let delta = &content[cache.raw_content.len()..];
        cache.raw_content = content.to_string();

        // Split delta into segments by newline
        let segments: Vec<&str> = delta.split('\n').collect();

        if cache.highlighted_lines.is_empty() {
            cache.highlighted_lines.push(plain_line(""));
            cache.last_line_raw.clear();
        }

        // First segment extends the last cached line (O(1) via cached last_line_raw)
        let last_idx = cache.highlighted_lines.len() - 1;
        cache.last_line_raw.push_str(strip_cr(segments[0]));
        cache.highlighted_lines[last_idx] =
            syntax::highlight_single_line(&cache.last_line_raw, &cache.lang)
                .unwrap_or_else(|| plain_line(&cache.last_line_raw));

        // Remaining segments are new lines
        for (i, segment) in segments[1..].iter().enumerate() {
            let clean = strip_cr(segment);
            cache.highlighted_lines.push(
                syntax::highlight_single_line(clean, &cache.lang)
                    .unwrap_or_else(|| plain_line(clean)),
            );
            // Track last_line_raw for the final segment
            if i == segments.len() - 2 {
                cache.last_line_raw = clean.to_string();
            }
        }

        Some(cache)
    }
}

/// Create a plain (unstyled) MarkdownLine.
///
/// Kind is [`SegmentKind::CodeBlock`] to stay consistent with the
/// syntax-highlighted lines this fallback renders alongside.
fn plain_line(text: &str) -> MarkdownLine {
    let mut line = MarkdownLine::default();
    line.push_segment(SegmentKind::CodeBlock, Style::default(), text);
    line
}

// ── ResultStrategy ──────────────────────────────────────────────

/// How to render a successful tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStrategy {
    /// Don't render result at all.
    Hidden,
    /// Render result in full (no truncation).
    Full,
    /// Head/tail with ellipsis (current default).
    Truncated,
}

// ── ToolRenderer ────────────────────────────────────────────────

/// Tool-specific rendering dispatch. Each variant defines how the tool's
/// header args and result strategy are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolRenderer {
    Bash,
    Read,
    Write,
    Edit,
    Glob,
    Grep,
    AskUser,
    /// Mostly unreachable in `result_strategy` — TodoWrite results are
    /// intercepted by `App::handle_tool_result` and rendered as `TodoMessage`
    /// cells. Only the pending state briefly uses this variant.
    TodoWrite,
    Fallback,
}

impl ToolRenderer {
    fn from_name(name: &str) -> Self {
        match name {
            constants::TOOL_BASH => Self::Bash,
            constants::TOOL_READ => Self::Read,
            constants::TOOL_WRITE => Self::Write,
            constants::TOOL_EDIT => Self::Edit,
            constants::TOOL_GLOB => Self::Glob,
            constants::TOOL_GREP => Self::Grep,
            constants::TOOL_ASK => Self::AskUser,
            constants::TOOL_TODO => Self::TodoWrite,
            _ => Self::Fallback,
        }
    }

    /// Build the args portion of the header, e.g. `"(path)"` or `"(path, pattern)"`.
    ///
    /// Returns empty string when there's nothing meaningful to display.
    /// The returned string does NOT include a leading space — the caller
    /// controls spacing.
    fn header_args(&self, args: &serde_json::Value) -> String {
        match self {
            Self::Bash => {
                let cmd = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(command_one_line)
                    .unwrap_or_default();
                if cmd.is_empty() {
                    String::new()
                } else {
                    format!("({cmd})")
                }
            }
            Self::Read | Self::Write | Self::Edit => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| truncate_path_segments(p, PATH_SEGMENT_COUNT))
                    .unwrap_or_default();
                if path.is_empty() {
                    String::new()
                } else {
                    format!("({path})")
                }
            }
            Self::Glob | Self::Grep => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| truncate_path_segments(p, PATH_SEGMENT_COUNT))
                    .unwrap_or_default();
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                match (path.is_empty(), pattern.is_empty()) {
                    (true, true) => String::new(),
                    (true, false) => format!("({pattern})"),
                    (false, true) => format!("({path})"),
                    (false, false) => format!("({path}, {pattern})"),
                }
            }
            Self::AskUser => String::new(),
            Self::TodoWrite => {
                let Some(todos) = args.get("todos").and_then(|v| v.as_array()) else {
                    return String::new();
                };
                let total = todos.len();
                if total == 0 {
                    return String::new();
                }
                let open = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) != Some("completed"))
                    .count();
                format!("({total} items · {open} open)")
            }
            Self::Fallback => fallback_args_summary(args),
        }
    }

    /// Result display strategy for successful tool calls.
    fn result_strategy(&self) -> ResultStrategy {
        match self {
            Self::Read | Self::Write | Self::Edit | Self::Glob | Self::Grep | Self::TodoWrite => {
                ResultStrategy::Hidden
            }
            Self::AskUser => ResultStrategy::Full,
            Self::Bash | Self::Fallback => ResultStrategy::Truncated,
        }
    }
}

// ── ToolCallBlock ───────────────────────────────────────────────

/// A tool invocation block showing name, args summary, and result.
#[derive(Debug, Clone)]
pub struct ToolCallBlock {
    pub tool_name: String,
    pub tool_args: serde_json::Value,
    pub tool_call_id: String,
    pub status: ToolStatus,
    pub result: Option<String>,
    /// When the tool started executing (for Bash timer display).
    pub started_at: Option<Instant>,
    /// Incremental syntax highlight cache for Write/Edit streaming.
    /// Write: file content preview. Edit: new_string preview.
    /// Mutually exclusive per tool — a block is never both Write and Edit.
    pub stream_highlight: Option<WriteHighlightCache>,
    /// Edit streaming: old_string lines (rendered red, no highlight).
    edit_old_lines: Vec<String>,
    /// TodoWrite streaming: partial todo list parsed from streaming args.
    todo_stream: Option<TodoMessage>,
    /// Raw args text accumulated from streaming fragments. Cleared when
    /// authoritative args arrive (`set_final_args`) to release memory.
    args_buffer: String,
}

impl ToolCallBlock {
    pub fn new(tool_name: String, tool_args: serde_json::Value, tool_call_id: String) -> Self {
        Self {
            tool_name,
            tool_args,
            tool_call_id,
            status: ToolStatus::Pending,
            result: None,
            started_at: None,
            stream_highlight: None,
            edit_old_lines: Vec::new(),
            todo_stream: None,
            args_buffer: String::new(),
        }
    }

    /// Create a streaming cell. Args arrive via `append_args_fragment`
    /// and are parsed locally — the backend sends raw text only.
    pub fn new_streaming(tool_name: String, tool_call_id: String) -> Self {
        Self {
            tool_name,
            tool_args: serde_json::Value::Object(serde_json::Map::new()),
            tool_call_id,
            status: ToolStatus::Streaming,
            result: None,
            started_at: None,
            stream_highlight: None,
            edit_old_lines: Vec::new(),
            todo_stream: None,
            args_buffer: String::new(),
        }
    }

    /// Set the result and status.
    pub fn set_result(&mut self, result: String, success: bool) {
        self.result = Some(result);
        self.status = if success {
            ToolStatus::Success
        } else {
            ToolStatus::Failed
        };
    }

    /// Append a raw args fragment (ToolCallStreamEvent) and re-parse the
    /// accumulated buffer for rendering.
    pub fn append_args_fragment(&mut self, fragment: &str) {
        self.args_buffer.push_str(fragment);
        let parsed = crate::util::partial_json::parse_streaming_json(&self.args_buffer);
        // Tool args are always a JSON object; ignore malformed non-object partials.
        let args = match parsed {
            obj @ serde_json::Value::Object(_) => obj,
            _ => serde_json::Value::Object(serde_json::Map::new()),
        };
        self.apply_args(args);
    }

    /// Set authoritative parsed args (execution start), transition to
    /// Pending, and release all streaming state. Self-contained — callers
    /// do not need a separate status transition.
    pub fn set_final_args(&mut self, args: serde_json::Value) {
        self.status = ToolStatus::Pending;
        self.args_buffer = String::new();
        self.stream_highlight = None;
        self.edit_old_lines.clear();
        self.todo_stream = None;
        self.tool_args = args;
    }

    /// Shared args application: refresh streaming caches, store args.
    fn apply_args(&mut self, args: serde_json::Value) {
        match self.tool_name.as_str() {
            constants::TOOL_WRITE => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !content.is_empty() {
                    self.stream_highlight =
                        WriteHighlightCache::update(self.stream_highlight.take(), path, content);
                }
            }
            constants::TOOL_EDIT => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let old_string = args
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let new_string = args
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                // old_string: full re-split each time (typically short).
                // Use split('\n') + strip_cr to match WriteHighlightCache semantics.
                if !old_string.is_empty() {
                    self.edit_old_lines = old_string
                        .split('\n')
                        .map(|l| strip_cr(l).to_string())
                        .collect();
                }

                // new_string: incremental highlight (shares stream_highlight cache;
                // Write and Edit are mutually exclusive per block).
                if !new_string.is_empty() {
                    self.stream_highlight =
                        WriteHighlightCache::update(self.stream_highlight.take(), path, new_string);
                }
            }
            constants::TOOL_TODO => {
                self.todo_stream = TodoMessage::from_tool_args(&args);
            }
            _ => {}
        }
        self.tool_args = args;
    }

    /// Render to lines.
    pub fn to_lines(&self, palette: &ThemePalette, max_output: usize) -> Vec<Line<'static>> {
        let renderer = ToolRenderer::from_name(&self.tool_name);
        let status = self.status;
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(palette.dim);

        // Header: ⦁ ToolName(args) [timer]
        // Args use the same bold style as the tool name.
        // Timer (Bash only) uses dim style.
        let args_part = renderer.header_args(&self.tool_args);

        let mut header_spans = vec![
            Span::styled(status.bullet(), Style::default().fg(status.color(palette))),
            Span::raw(" "),
            Span::styled(self.tool_name.clone(), bold),
            Span::styled(args_part, bold),
        ];

        // Bash timer: appended as dim text after args.
        if renderer == ToolRenderer::Bash {
            let timer = format_bash_timer(self.started_at, &self.tool_args);
            if !timer.is_empty() {
                header_spans.push(Span::raw(" "));
                header_spans.push(Span::styled(timer, dim));
            }
        }

        let header = Line::from(header_spans);
        let mut lines = vec![header];

        // Write streaming: render highlighted content preview.
        if status == ToolStatus::Streaming
            && renderer == ToolRenderer::Write
            && let Some(ref cache) = self.stream_highlight
        {
            let total = cache.highlighted_lines.len();
            let show_count = total.min(WRITE_STREAM_MAX_LINES);
            let start = total.saturating_sub(show_count);
            if start > 0 {
                let dim = Style::default().fg(palette.dim);
                lines.push(Line::from(Span::styled(
                    format!("    … +{start} lines"),
                    dim,
                )));
            }
            for hl_line in &cache.highlighted_lines[start..] {
                let mut spans: Vec<Span<'static>> =
                    vec![Span::styled("    ", Style::default().fg(palette.dim))];
                for seg in &hl_line.segments {
                    spans.push(Span::styled(seg.text.clone(), seg.style));
                }
                lines.push(Line::from(spans));
            }
        }

        // Edit streaming: render live diff preview (old_string red, new_string green).
        if status == ToolStatus::Streaming && renderer == ToolRenderer::Edit {
            let danger = Style::default().fg(palette.danger);
            let dim_style = Style::default().fg(palette.dim);

            // old_string lines (red, no syntax highlight).
            if !self.edit_old_lines.is_empty() {
                let total = self.edit_old_lines.len();
                if total > EDIT_STREAM_MAX_OLD_LINES {
                    // Collapse: keep head + tail, insert gap indicator.
                    let keep = EDIT_STREAM_OLD_COLLAPSE_KEEP;
                    debug_assert!(
                        keep * 2 < EDIT_STREAM_MAX_OLD_LINES,
                        "KEEP*2 must be < MAX_OLD to avoid overlap/underflow"
                    );
                    for line in &self.edit_old_lines[..keep] {
                        lines.push(Line::from(vec![
                            Span::styled("    ", dim_style),
                            Span::styled(format!("- {line}"), danger),
                        ]));
                    }
                    let omitted = total - keep * 2;
                    lines.push(Line::from(Span::styled(
                        format!("    ⋮ {omitted} lines"),
                        dim_style,
                    )));
                    for line in &self.edit_old_lines[total - keep..] {
                        lines.push(Line::from(vec![
                            Span::styled("    ", dim_style),
                            Span::styled(format!("- {line}"), danger),
                        ]));
                    }
                } else {
                    for line in &self.edit_old_lines {
                        lines.push(Line::from(vec![
                            Span::styled("    ", dim_style),
                            Span::styled(format!("- {line}"), danger),
                        ]));
                    }
                }
            }

            // new_string lines (green, syntax highlighted).
            if let Some(ref cache) = self.stream_highlight {
                let total = cache.highlighted_lines.len();
                let show_count = total.min(EDIT_STREAM_MAX_NEW_LINES);
                let start = total.saturating_sub(show_count);
                if start > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("    ⋮ {start} lines"),
                        dim_style,
                    )));
                }
                let success = Style::default().fg(palette.success);
                for hl_line in &cache.highlighted_lines[start..] {
                    let mut spans: Vec<Span<'static>> =
                        vec![Span::styled("    ", dim_style), Span::styled("+ ", success)];
                    for seg in &hl_line.segments {
                        spans.push(Span::styled(seg.text.clone(), seg.style));
                    }
                    lines.push(Line::from(spans));
                }
            }
        }

        // TodoWrite streaming: render partial todo list.
        if status == ToolStatus::Streaming
            && renderer == ToolRenderer::TodoWrite
            && let Some(ref todo) = self.todo_stream
        {
            lines.extend(render_todo_items(&todo.items, palette));
        }

        // Result rendering.
        if let Some(ref result) = self.result {
            match status {
                ToolStatus::Failed => {
                    // All tools: truncated error message.
                    let truncated = truncate_by_chars(result, FAILED_RESULT_MAX_CHARS);
                    lines.push(result_line(&truncated, "  └ ", palette));
                }
                ToolStatus::Success => match renderer.result_strategy() {
                    ResultStrategy::Hidden => {
                        // No result lines.
                    }
                    ResultStrategy::Full => {
                        // Render all result lines.
                        for (i, raw) in result.lines().enumerate() {
                            let prefix = if i == 0 { "  └ " } else { "    " };
                            lines.push(result_line(raw, prefix, palette));
                        }
                    }
                    ResultStrategy::Truncated => {
                        render_truncated_result(&mut lines, result, max_output, palette);
                    }
                },
                ToolStatus::Pending | ToolStatus::Streaming => {
                    // Pending/streaming tools don't have results yet.
                }
            }
        }

        lines.push(Line::from(""));
        lines
    }
}

// ── Path / command helpers ──────────────────────────────────────

/// Truncate a path to the last `n` segments.
///
/// "/Users/abiter/Documents/ws/mine/OpenWing/libs/core/wing/tools/bash.py"
/// with n=6 → "OpenWing/libs/core/wing/tools/bash.py"
///
/// If the path has ≤ n segments, returns it unchanged.
fn truncate_path_segments(path: &str, n: usize) -> String {
    // Normalize: strip trailing slash.
    let path = path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() <= n {
        return path.to_string();
    }
    segments[segments.len() - n..].join("/")
}

/// Collapse a multi-line command into a single line without truncation.
///
/// Replaces `\r\n` and `\n` with `⏎`. The full command is preserved —
/// terminal wrapping handles long lines.
fn command_one_line(cmd: &str) -> String {
    cmd.trim().replace("\r\n", "⏎").replace('\n', "⏎")
}

/// Format the Bash timer display: "3s/30s" or "3s" or empty.
fn format_bash_timer(started_at: Option<Instant>, args: &serde_json::Value) -> String {
    let Some(started) = started_at else {
        return String::new();
    };
    let elapsed = started.elapsed().as_secs();
    if let Some(timeout) = args.get("timeout").and_then(|v| v.as_u64()) {
        format!("{elapsed}s/{timeout}s")
    } else {
        format!("{elapsed}s")
    }
}

/// Fallback args summary for unknown tools (key=value pairs, truncated).
///
/// Returns the summary WITHOUT a leading space — caller controls spacing.
fn fallback_args_summary(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .filter(|(k, _)| k.as_str() != "purpose")
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => truncate_by_chars(s, 50),
                        other => {
                            let s = other.to_string();
                            truncate_by_chars(&s, 50)
                        }
                    };
                    format!("{k}={val}")
                })
                .collect();
            if parts.is_empty() {
                String::new()
            } else {
                parts.join(" ")
            }
        }
        _ => String::new(),
    }
}

/// Render truncated result (head/tail with ellipsis).
fn render_truncated_result(
    lines: &mut Vec<Line<'static>>,
    result: &str,
    max_output: usize,
    palette: &ThemePalette,
) {
    let result_lines: Vec<&str> = result.lines().collect();
    let total = result_lines.len();

    if total <= max_output * 2 {
        for (i, raw) in result_lines.iter().enumerate() {
            let prefix = if i == 0 { "  └ " } else { "    " };
            lines.push(result_line(raw, prefix, palette));
        }
    } else {
        for (i, raw) in result_lines[..max_output].iter().enumerate() {
            let prefix = if i == 0 { "  └ " } else { "    " };
            lines.push(result_line(raw, prefix, palette));
        }
        let omitted = total - max_output * 2;
        let dim = Style::default().fg(palette.dim);
        lines.push(Line::from(Span::styled(
            format!("    … +{omitted} lines"),
            dim,
        )));
        for raw in &result_lines[total - max_output..] {
            lines.push(result_line(raw, "    ", palette));
        }
    }
}

/// Build a result line with prefix and tool_result style.
fn result_line(text: &str, prefix: &str, palette: &ThemePalette) -> Line<'static> {
    Line::from(vec![
        Span::styled(prefix.to_string(), Style::default().fg(palette.dim)),
        Span::styled(text.to_string(), Style::default().fg(palette.tool_result)),
    ])
}

/// Truncate a string by character count (not bytes), adding "..." if truncated.
///
/// Safe for UTF-8: operates on chars, not bytes.
pub fn truncate_by_chars(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemePalette;
    use serde_json::json;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    fn lines_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_tool_call_pending() {
        let block = ToolCallBlock::new("Read".into(), json!({"path": "main.rs"}), "tc1".into());
        let lines = block.to_lines(&p(), 10);
        let text = lines_text(&lines);
        assert!(text.contains("⦁"), "missing bullet prefix: {text}");
        assert!(text.contains("Read"), "missing tool name: {text}");
        assert!(text.contains("main.rs"), "missing path: {text}");
        // No space between tool name and args.
        assert!(text.contains("Read(main.rs)"), "unexpected space: {text}");
    }

    #[test]
    fn test_bash_with_timeout() {
        let mut block = ToolCallBlock::new(
            "Bash".into(),
            json!({"command": "ls -la", "timeout": 30}),
            "tc2".into(),
        );
        block.started_at = Some(Instant::now());
        block.set_result("file1\nfile2".into(), true);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("Bash"), "missing tool name: {text}");
        assert!(text.contains("ls -la"), "missing command: {text}");
        assert!(text.contains("/30s"), "missing timeout: {text}");
        assert!(text.contains("file1"), "missing result: {text}");
        // No space between tool name and args.
        assert!(text.contains("Bash(ls -la)"), "unexpected space: {text}");
    }

    #[test]
    fn test_bash_without_timeout() {
        let mut block = ToolCallBlock::new(
            "Bash".into(),
            json!({"command": "echo hello"}),
            "tc3".into(),
        );
        block.started_at = Some(Instant::now());
        block.set_result("hello".into(), true);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("Bash"), "missing tool name: {text}");
        assert!(text.contains("echo hello"), "missing command: {text}");
        assert!(
            !text.contains("/"),
            "should not have timeout separator: {text}"
        );
    }

    #[test]
    fn test_bash_command_not_truncated() {
        let long_cmd = "echo ".to_string() + &"x".repeat(200);
        let block = ToolCallBlock::new(
            "Bash".into(),
            json!({"command": long_cmd.clone()}),
            "tc_cmd".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        // Full command should be present (not truncated).
        assert!(text.contains(&long_cmd), "command should not be truncated");
    }

    #[test]
    fn test_bash_multiline_command() {
        let block = ToolCallBlock::new(
            "Bash".into(),
            json!({"command": "echo hello\necho world"}),
            "tc_ml".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("echo hello⏎echo world"),
            "multiline should be collapsed: {text}"
        );
    }

    #[test]
    fn test_bash_crlf_command() {
        let block = ToolCallBlock::new(
            "Bash".into(),
            json!({"command": "echo hello\r\necho world"}),
            "tc_crlf".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("echo hello⏎echo world"),
            "CRLF should be handled: {text}"
        );
        assert!(!text.contains('\r'), "no stray \\r: {text}");
    }

    #[test]
    fn test_read_success_no_result() {
        let mut block =
            ToolCallBlock::new("Read".into(), json!({"path": "/a/b/c.rs"}), "tc4".into());
        block.set_result("file contents here".into(), true);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("Read"), "missing tool name: {text}");
        assert!(
            !text.contains("file contents"),
            "result should be hidden: {text}"
        );
    }

    #[test]
    fn test_write_success_no_result() {
        let mut block = ToolCallBlock::new(
            "Write".into(),
            json!({"path": "/a/b/c.rs", "content": "hello"}),
            "tc5".into(),
        );
        block.set_result("write: ok".into(), true);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            !text.contains("write: ok"),
            "result should be hidden: {text}"
        );
    }

    #[test]
    fn test_tool_call_failed_truncated() {
        let mut block = ToolCallBlock::new("Bash".into(), json!({}), "tc6".into());
        let long_error = "error: ".to_string() + &"x".repeat(100);
        block.set_result(long_error, false);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("⦁"), "missing bullet prefix: {text}");
        assert!(text.contains("error:"), "missing error prefix: {text}");
        // Should be truncated.
        assert!(text.contains("..."), "should be truncated: {text}");
    }

    #[test]
    fn test_ask_user_no_args_in_header() {
        let block = ToolCallBlock::new(
            "AskUserQuestion".into(),
            json!({"question": "Which?", "choices": ["A", "B"]}),
            "tc7".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("AskUserQuestion"),
            "missing tool name: {text}"
        );
        assert!(
            !text.contains("Which?"),
            "should not show question in header: {text}"
        );
    }

    #[test]
    fn test_ask_user_result_full() {
        let mut block = ToolCallBlock::new(
            "AskUserQuestion".into(),
            json!({"question": "Which?"}),
            "tc8".into(),
        );
        block.set_result("I prefer option B because it's simpler".into(), true);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("I prefer option B"),
            "should show full user feedback: {text}"
        );
    }

    #[test]
    fn test_glob_with_pattern() {
        let block = ToolCallBlock::new(
            "Glob".into(),
            json!({"path": "/Users/abiter/project", "pattern": "**/*.py"}),
            "tc9".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("Glob"), "missing tool name: {text}");
        assert!(text.contains("**/*.py"), "missing pattern: {text}");
    }

    #[test]
    fn test_glob_empty_path() {
        let block = ToolCallBlock::new(
            "Glob".into(),
            json!({"pattern": "**/*.py"}),
            "tc_glob_empty".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        // Should show just the pattern, not "(, **/*.py)".
        assert!(
            !text.contains("(, "),
            "should not have leading comma: {text}"
        );
        assert!(
            text.contains("(**/*.py)"),
            "should show pattern only: {text}"
        );
    }

    #[test]
    fn test_grep_with_pattern() {
        let block = ToolCallBlock::new(
            "Grep".into(),
            json!({"path": ".", "pattern": "fn main"}),
            "tc10".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("Grep"), "missing tool name: {text}");
        assert!(text.contains("fn main"), "missing pattern: {text}");
    }

    #[test]
    fn test_todo_write_no_args() {
        let block = ToolCallBlock::new(
            "TodoWrite".into(),
            json!({"todos": [{"content": "test", "status": "pending"}]}),
            "tc11".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("TodoWrite"), "missing tool name: {text}");
        assert!(!text.contains("todos"), "should not show args: {text}");
    }

    #[test]
    fn test_fallback_generic() {
        let block = ToolCallBlock::new(
            "CustomTool".into(),
            json!({"foo": "bar", "baz": "qux"}),
            "tc12".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("CustomTool"), "missing tool name: {text}");
        assert!(text.contains("foo=bar"), "missing fallback args: {text}");
        // No double space between tool name and args.
        assert!(
            !text.contains("CustomTool  "),
            "double space detected: {text}"
        );
    }

    #[test]
    fn test_truncate_path_segments() {
        assert_eq!(
            truncate_path_segments(
                "/Users/abiter/Documents/ws/mine/OpenWing/libs/core/wing/tools/bash.py",
                6
            ),
            "OpenWing/libs/core/wing/tools/bash.py"
        );
        assert_eq!(truncate_path_segments("src/main.rs", 6), "src/main.rs");
        assert_eq!(truncate_path_segments("main.rs", 6), "main.rs");
        assert_eq!(truncate_path_segments("/a/b/c/d/e/f/g/h", 3), "f/g/h");
    }

    #[test]
    fn test_command_one_line() {
        assert_eq!(command_one_line("ls -la"), "ls -la");
        assert_eq!(
            command_one_line("echo hello\necho world"),
            "echo hello⏎echo world"
        );
        assert_eq!(
            command_one_line("echo hello\r\necho world"),
            "echo hello⏎echo world"
        );
    }

    #[test]
    fn test_truncate_by_chars_ascii() {
        assert_eq!(truncate_by_chars("hello", 10), "hello");
        assert_eq!(truncate_by_chars("hello world!", 8), "hello...");
    }

    #[test]
    fn test_truncate_by_chars_cjk_no_panic() {
        let cjk = "你好世界这是一段中文文本";
        let result = truncate_by_chars(cjk, 10);
        assert!(result.len() <= cjk.len());
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_edit_path_display() {
        let block = ToolCallBlock::new(
            "Edit".into(),
            json!({
                "path": "/Users/abiter/project/src/main.rs",
                "old_string": "old",
                "new_string": "new"
            }),
            "tc13".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("Edit"), "missing tool name: {text}");
        assert!(text.contains("main.rs"), "missing path: {text}");
        assert!(
            !text.contains("old_string"),
            "should not show old/new in header: {text}"
        );
        // No space between tool name and args.
        assert!(
            text.contains("Edit(") && !text.contains("Edit ("),
            "unexpected space before parens: {text}"
        );
    }

    #[test]
    fn test_result_prefix_consistency() {
        let mut block = ToolCallBlock::new("Bash".into(), json!({}), "tc14".into());
        block.set_result("line1\nline2\nline3".into(), true);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("└ line1"),
            "first line missing tree prefix: {text}"
        );
    }

    #[test]
    fn test_empty_args_graceful() {
        // No args at all — should not panic, degrade gracefully.
        let block = ToolCallBlock::new("Read".into(), json!({}), "tc15".into());
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("Read"), "missing tool name: {text}");
        // Empty path → no parens, just "Read".
        assert!(
            !text.contains("()"),
            "empty parens should not appear: {text}"
        );
    }

    #[test]
    fn test_streaming_status_hollow_bullet() {
        let mut block = ToolCallBlock::new(
            "Edit".into(),
            json!({"path": "src/main.rs"}),
            "tc_s1".into(),
        );
        block.status = ToolStatus::Streaming;
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("◌"),
            "streaming should use hollow bullet: {text}"
        );
        assert!(
            !text.contains("⦁"),
            "streaming should not use solid bullet: {text}"
        );
        assert!(text.contains("Edit"), "missing tool name: {text}");
    }

    #[test]
    fn test_pending_status_solid_bullet() {
        let block = ToolCallBlock::new(
            "Edit".into(),
            json!({"path": "src/main.rs"}),
            "tc_s2".into(),
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("⦁"),
            "pending should use solid bullet: {text}"
        );
        assert!(
            !text.contains("◌"),
            "pending should not use hollow bullet: {text}"
        );
    }

    #[test]
    fn test_append_args_fragment() {
        let mut block = ToolCallBlock::new_streaming("Bash".into(), "tc_s3".into());
        // Fragments accumulate and are partial-parsed for rendering.
        block.append_args_fragment(r#"{"command": "ls"#);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("ls"),
            "partial args should be visible: {text}"
        );

        block.append_args_fragment(r#" -la"}"#);
        assert_eq!(block.tool_args["command"], "ls -la");
    }

    #[test]
    fn test_set_final_args_releases_buffer() {
        let mut block = ToolCallBlock::new_streaming("Bash".into(), "tc_s5".into());
        block.append_args_fragment(r#"{"command": "ls"#);
        assert!(!block.args_buffer.is_empty());

        block.set_final_args(json!({"command": "ls -la"}));
        assert!(
            block.args_buffer.is_empty(),
            "streaming buffer should be released"
        );
        assert_eq!(block.tool_args["command"], "ls -la");
    }

    #[test]
    fn test_write_streaming_highlight() {
        let mut block = ToolCallBlock::new_streaming("Write".into(), "tc_s4".into());
        // Simulate streaming via raw fragments (backend sends unparsed text).
        block.append_args_fragment(r#"{"path": "/tmp/test.py", "content": "def main"#);
        block.append_args_fragment(r#"():\n    print('hello')"}"#);
        let text = lines_text(&block.to_lines(&p(), 30));
        assert!(text.contains("Write"), "missing tool name: {text}");
        assert!(
            text.contains("◌"),
            "streaming should use hollow bullet: {text}"
        );
        // Content should be rendered (highlighted or plain)
        assert!(
            text.contains("def main()") || text.contains("print"),
            "streaming content should be visible: {text}"
        );
    }

    #[test]
    fn test_stream_highlight_cache_incremental() {
        // First update: full build
        let cache = WriteHighlightCache::update(None, "/tmp/test.py", "line1\nline2");
        assert!(cache.is_some());
        let cache = cache.unwrap();
        assert_eq!(cache.highlighted_lines.len(), 2);

        // Incremental update: append a line
        let cache2 =
            WriteHighlightCache::update(Some(cache), "/tmp/test.py", "line1\nline2\nline3");
        assert!(cache2.is_some());
        let cache2 = cache2.unwrap();
        assert_eq!(cache2.highlighted_lines.len(), 3);

        // No change
        let cache3 = WriteHighlightCache::update(
            Some(cache2.clone()),
            "/tmp/test.py",
            "line1\nline2\nline3",
        );
        assert!(cache3.is_some());
        assert_eq!(cache3.unwrap().highlighted_lines.len(), 3);
    }

    #[test]
    fn test_stream_highlight_cache_unknown_ext() {
        // Unknown extension → falls back to plain text (lang=""), still renders.
        let cache = WriteHighlightCache::update(None, "/tmp/file.xyz_unknown", "content");
        assert!(cache.is_some());
        let cache = cache.unwrap();
        assert_eq!(cache.highlighted_lines.len(), 1);
        assert_eq!(cache.lang, "");
    }

    #[test]
    fn test_stream_highlight_cache_empty_path_fallback() {
        // Empty path (LLM emits content before path) → plain text fallback.
        let cache = WriteHighlightCache::update(None, "", "line1\nline2");
        assert!(cache.is_some());
        let cache = cache.unwrap();
        assert_eq!(cache.highlighted_lines.len(), 2);
        assert_eq!(cache.lang, "");

        // When real path arrives, lang mismatch triggers full rebuild.
        let cache2 = WriteHighlightCache::update(Some(cache), "/tmp/t.py", "line1\nline2\nline3");
        assert!(cache2.is_some());
        let cache2 = cache2.unwrap();
        assert_ne!(cache2.lang, ""); // Now has real syntax
        assert_eq!(cache2.highlighted_lines.len(), 3);
    }

    #[test]
    fn test_stream_highlight_cache_trailing_newline() {
        // P0 regression: trailing '\n' must produce an extra empty line
        let cache = WriteHighlightCache::update(None, "/tmp/t.py", "line1\n");
        assert!(cache.is_some());
        let cache = cache.unwrap();
        // "line1\n".split('\n') → ["line1", ""] → 2 lines
        assert_eq!(cache.highlighted_lines.len(), 2);

        // Incremental: append after trailing newline
        let cache2 = WriteHighlightCache::update(Some(cache), "/tmp/t.py", "line1\nl");
        assert!(cache2.is_some());
        let cache2 = cache2.unwrap();
        // Should be ["line1", "l"] — NOT ["line1l"]
        assert_eq!(cache2.highlighted_lines.len(), 2);
        assert_eq!(cache2.last_line_raw, "l");
    }

    #[test]
    fn test_stream_highlight_cache_crlf() {
        // CRLF content: \r should be stripped
        let cache = WriteHighlightCache::update(None, "/tmp/t.py", "line1\r\nline2\r\n");
        assert!(cache.is_some());
        let cache = cache.unwrap();
        // "line1\r\nline2\r\n".split('\n') → ["line1\r", "line2\r", ""] → 3 lines
        assert_eq!(cache.highlighted_lines.len(), 3);
        assert_eq!(cache.last_line_raw, "");

        // Incremental after CRLF
        let cache2 = WriteHighlightCache::update(Some(cache), "/tmp/t.py", "line1\r\nline2\r\nx");
        assert!(cache2.is_some());
        let cache2 = cache2.unwrap();
        assert_eq!(cache2.highlighted_lines.len(), 3);
        assert_eq!(cache2.last_line_raw, "x");
    }

    // ── Edit streaming tests ─────────────────────────────────────

    #[test]
    fn test_edit_streaming_old_string_only() {
        let mut block = ToolCallBlock::new_streaming("Edit".into(), "tc_e1".into());
        block.append_args_fragment(
            r#"{"path": "src/main.rs", "old_string": "fn old() {\n    println!(\"old\");\n}"#,
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("◌"), "streaming bullet: {text}");
        assert!(text.contains("Edit(src/main.rs)"), "header: {text}");
        assert!(text.contains("- fn old()"), "old_string line: {text}");
        assert!(text.contains("- }"), "old_string last line: {text}");
        // No new_string yet — no "+ " prefixed lines.
        assert!(!text.contains("+ fn"), "no new_string yet: {text}");
    }

    #[test]
    fn test_edit_streaming_both_strings() {
        let mut block = ToolCallBlock::new_streaming("Edit".into(), "tc_e2".into());
        block.append_args_fragment(
            r#"{"path": "src/main.rs", "old_string": "fn old() {}", "new_string": "fn new() {\n    todo!()\n}"#,
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("- fn old() {}"), "old_string: {text}");
        assert!(text.contains("+ fn new()"), "new_string first line: {text}");
        assert!(text.contains("+ }"), "new_string last line: {text}");
    }

    #[test]
    fn test_edit_streaming_new_string_only() {
        // LLM might emit new_string before old_string.
        let mut block = ToolCallBlock::new_streaming("Edit".into(), "tc_e3".into());
        block.append_args_fragment(r#"{"path": "a.rs", "new_string": "hello world"#);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            text.contains("+ hello world"),
            "new_string without old: {text}"
        );
        // No old_string lines (no "- " prefixed content lines).
        assert!(!text.contains("- hello"), "no old_string lines: {text}");
    }

    #[test]
    fn test_edit_streaming_old_string_collapse() {
        // old_string > 8 lines should collapse.
        let old_lines: Vec<String> = (1..=12).map(|i| format!("line {i}")).collect();
        let old_string = old_lines.join("\n");
        let fragment = format!(
            r#"{{"path": "a.rs", "old_string": "{}"}}"#,
            old_string.replace('\n', "\\n")
        );
        let mut block = ToolCallBlock::new_streaming("Edit".into(), "tc_e4".into());
        block.append_args_fragment(&fragment);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("⋮"), "should collapse: {text}");
        assert!(text.contains("- line 1"), "head kept: {text}");
        assert!(text.contains("- line 12"), "tail kept: {text}");
        assert!(!text.contains("- line 5"), "middle hidden: {text}");
    }

    #[test]
    fn test_edit_streaming_disappears_on_pending() {
        let mut block = ToolCallBlock::new_streaming("Edit".into(), "tc_e5".into());
        block.append_args_fragment(r#"{"path": "a.rs", "old_string": "old", "new_string": "new"#);
        // Streaming: preview visible.
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("- old"), "visible during streaming: {text}");

        // Transition to Pending (ToolCall event arrives).
        block.set_final_args(json!({"path": "a.rs", "old_string": "old", "new_string": "new"}));
        // Caches are released.
        assert!(block.edit_old_lines.is_empty(), "old_lines cleared");
        assert!(block.stream_highlight.is_none(), "highlight cleared");
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(
            !text.contains("- old"),
            "preview gone after pending: {text}"
        );
        assert!(
            !text.contains("+ new"),
            "preview gone after pending: {text}"
        );
    }

    #[test]
    fn test_edit_streaming_trailing_newline_consistency() {
        // old_string and new_string with trailing \n should produce symmetric lines.
        let mut block = ToolCallBlock::new_streaming("Edit".into(), "tc_e6".into());
        block.append_args_fragment(r#"{"path": "a.rs", "old_string": "a\n", "new_string": "b\n"}"#);
        let text = lines_text(&block.to_lines(&p(), 10));
        // Both use split('\n'): "a\n" → ["a", ""], "b\n" → ["b", ""]
        let old_count = text.matches("- ").count();
        let new_count = text.matches("+ ").count();
        assert_eq!(old_count, new_count, "symmetric lines: {text}");
        assert_eq!(old_count, 2, "trailing newline produces 2 lines: {text}");
    }

    #[test]
    fn test_edit_streaming_crlf() {
        // CRLF in old_string: \r should be stripped.
        let mut block = ToolCallBlock::new_streaming("Edit".into(), "tc_e7".into());
        block.append_args_fragment(r#"{"path": "a.rs", "old_string": "line1\r\nline2\r\n"}"#);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("- line1"), "CRLF stripped: {text}");
        assert!(text.contains("- line2"), "CRLF stripped: {text}");
        assert!(!text.contains('\r'), "no stray \\r: {text}");
    }

    // ── TodoWrite streaming tests ────────────────────────────────

    #[test]
    fn test_todo_streaming_partial_items() {
        let mut block = ToolCallBlock::new_streaming("TodoWrite".into(), "tc_t1".into());
        block.append_args_fragment(
            r#"{"todos": [{"content": "task 1", "status": "completed"}, {"content": "task 2", "status": "in_progress", "activeForm": "Doing task 2"}"#,
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("◌"), "streaming bullet: {text}");
        assert!(text.contains("TodoWrite"), "tool name: {text}");
        assert!(text.contains("✓"), "completed icon: {text}");
        assert!(text.contains("task 1"), "completed content: {text}");
        assert!(text.contains("●"), "in_progress icon: {text}");
        assert!(text.contains("Doing task 2"), "activeForm: {text}");
    }

    #[test]
    fn test_todo_streaming_header_stats() {
        let mut block = ToolCallBlock::new_streaming("TodoWrite".into(), "tc_t2".into());
        block.append_args_fragment(
            r#"{"todos": [{"content": "a", "status": "completed"}, {"content": "b", "status": "pending"}, {"content": "c", "status": "in_progress"}]}"#,
        );
        let text = lines_text(&block.to_lines(&p(), 10));
        // 3 items, 2 open (pending + in_progress are both "open").
        assert!(text.contains("(3 items · 2 open)"), "header stats: {text}");
    }

    #[test]
    fn test_todo_streaming_disappears_on_pending() {
        let mut block = ToolCallBlock::new_streaming("TodoWrite".into(), "tc_t3".into());
        block.append_args_fragment(r#"{"todos": [{"content": "x", "status": "pending"}]}"#);
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(text.contains("○"), "visible during streaming: {text}");

        block.set_final_args(json!({"todos": [{"content": "x", "status": "pending"}]}));
        assert!(block.todo_stream.is_none(), "todo_stream cleared");
        let text = lines_text(&block.to_lines(&p(), 10));
        assert!(!text.contains("○"), "preview gone after pending: {text}");
    }

    #[test]
    fn test_todo_streaming_empty_todos() {
        let mut block = ToolCallBlock::new_streaming("TodoWrite".into(), "tc_t4".into());
        block.append_args_fragment(r#"{"todos": ["#);
        let text = lines_text(&block.to_lines(&p(), 10));
        // No items yet — just header, no crash.
        assert!(text.contains("TodoWrite"), "tool name: {text}");
    }
}
