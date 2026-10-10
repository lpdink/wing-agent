//! `wing tail` / `wing head` — message filtering (like Unix head/tail).
//!
//! Fetches session messages via `GET /api/session/get` and filters by element.
//! `tail` shows the last N, `head` shows the first N.
//!
//! Decoding lives in the shared typed mirror (`protocol::SessionMessage`);
//! filtering and printing run on the typed view. `--json` emits stripped
//! element records (`{uuid, …selected fields}`) for element filters and the
//! raw payloads verbatim for `all` (typed serialization would drop unknown
//! fields and change key order, so neither path round-trips through the
//! mirror).
//!
//! # `--filter`: one element set, every output derived from it
//!
//! `head` / `tail` treat the log as a flat sequence of elements — user text,
//! reasoning, assistant text, tool calls, tool results — and `--filter` names
//! the ones to show. Names are repeatable and comma-separated, and combine as
//! a **union**: `--filter user,content` is user text + assistant text and
//! nothing else. A row is printed iff it carries at least one selected
//! element, and only the selected elements are rendered — in text and in
//! `--json` alike.
//!
//! | `--filter`      | Selected elements                                       |
//! |-----------------|---------------------------------------------------------|
//! | `all` (default) | Every message, unfiltered (`--json`: payloads verbatim)  |
//! | `user`          | User-message text                                        |
//! | `assistant`     | A message's own elements (reasoning + text + tool calls) |
//! | `reasoning`     | Assistant `reasoning_content`                            |
//! | `content`       | Assistant text                                           |
//! | `tool_call`     | Tool calls (each line carries its call id)               |
//! | `tool_result`   | Tool results                                             |
//!
//! The parse result (`Filter` → `Selection`) is the **single source of
//! truth**: row selection, text rendering and the `--json` records all walk
//! that one element set through the same per-element definitions
//! (`Element::carried_by` / `Element::in_record`), so no output path can drift
//! from another. The vocabulary itself is a clap `ValueEnum`: an unknown value
//! is rejected before the request — never silently treated as "no filter"
//! (that fallback is what printed every section for `--filter user,content`).
//!
//! Element definitions (one per element, shared by every output path):
//!
//! - `user` is a user message's text; `content` is an assistant message's
//!   text. A tool message carries neither: its `content` *is* its tool result
//!   and renders through `tool_result` only, so every result appears exactly
//!   once. Selection is "the message carries the element", never message-level
//!   purity: a message carrying tool calls still yields its text.
//! - `reasoning` is a non-empty `reasoning_content` on an assistant message.
//! - `tool_call` is a non-empty `tool_calls` array — absent / null / `[]` are
//!   not a tool call.
//! - `tool_result` is every tool message: an empty result is still a result.
//!
//! `all` is the universe: a list containing it is unfiltered (`all,user` ≡
//! `all`), and a payload that does not decode into a Message projection
//! carries no element, so it stays reachable through `all` only.
//!
//! Tool results print as a **500-char peek** in text mode — that is the
//! tool-result element's single text rendering; `--json` carries the stored
//! content in full. Note the backend caps results over
//! `tool_result_truncate.max_length` (100k) *before* storing them, so beyond
//! that cap even the payload is the head/marker/tail form (the full text lives
//! in the temp file its marker names).

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::fmt;
use std::process::ExitCode;

use anyhow::Result;
use clap::ValueEnum;
use serde_json::{Map, Value, json};

use crate::protocol::SessionMessage;

use super::common;

/// `--filter` help text, shared by `tail` and `head` (one definition, so the
/// two commands cannot drift apart).
pub const FILTER_HELP: &str = "\
Filter by element(s): repeatable / comma-separated values combine as a union \
(`user,content` = user text + assistant text only). `assistant` selects a \
message's own elements (reasoning + content + tool_call); `all` is unfiltered. \
Only the selected elements are printed — in text and `--json` alike (tool \
results print as a 500-char peek in text mode).";

/// One `--filter` name as spelled on the command line: the CLI surface of the
/// element vocabulary (the `#[value(name = …)]` spellings are what `--help`
/// lists and what the command accepts).
///
/// A value outside this set is a clap error before any request — the old
/// hand-rolled matching accepted anything and quietly printed the unfiltered
/// view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FilterArg {
    #[value(name = "all")]
    All,
    #[value(name = "user")]
    User,
    #[value(name = "assistant")]
    Assistant,
    #[value(name = "tool_call")]
    ToolCall,
    #[value(name = "tool_result")]
    ToolResult,
    #[value(name = "reasoning")]
    Reasoning,
    #[value(name = "content")]
    Content,
}

/// One selectable element of the log — the unit `--filter` selects.
///
/// Declaration order is the canonical render order (which is also the order
/// the section checks, the body lines and the `--json` keys come out in). A
/// message owns the elements of its role only, so the two text elements never
/// both apply to one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Element {
    /// User-message text (`user`).
    UserText,
    /// Assistant `reasoning_content` (`reasoning`).
    Reasoning,
    /// Assistant text (`content`).
    Content,
    /// Assistant tool calls (`tool_call`).
    ToolCall,
    /// A tool message's result (`tool_result`).
    ToolResult,
}

impl Element {
    /// Every element, in canonical order.
    const ALL: [Element; 5] = [
        Self::UserText,
        Self::Reasoning,
        Self::Content,
        Self::ToolCall,
        Self::ToolResult,
    ];

    /// An assistant message's own elements — `assistant` is exactly their
    /// union, not a separate rule.
    const ASSISTANT: [Element; 3] = [Self::Reasoning, Self::Content, Self::ToolCall];

    /// Bit position in [`Selection`].
    fn index(self) -> u8 {
        match self {
            Self::UserText => 0,
            Self::Reasoning => 1,
            Self::Content => 2,
            Self::ToolCall => 3,
            Self::ToolResult => 4,
        }
    }

    /// The `--filter` name (canonical, comma-joined by `Filter`'s `Display`).
    fn name(self) -> &'static str {
        match self {
            Self::UserText => "user",
            Self::Reasoning => "reasoning",
            Self::Content => "content",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }

    /// The role that owns the element — the role whose message renders it.
    fn owner_role(self) -> &'static str {
        match self {
            Self::UserText => "user",
            Self::Reasoning | Self::Content | Self::ToolCall => "assistant",
            Self::ToolResult => "tool",
        }
    }

    /// Whether `msg` carries this element (row selection + text rendering).
    ///
    /// The one definition of element membership: the owning role, plus the
    /// element's own presence rule. Text elements need non-empty text (a
    /// tool-only message has no narration to strip, and empty reasoning is not
    /// reasoning); a tool result is its message, empty or not.
    fn carried_by(self, msg: &SessionMessage) -> bool {
        if msg.role != self.owner_role() {
            return false;
        }
        match self {
            Self::UserText | Self::ToolResult => true,
            Self::Reasoning => !reasoning(msg).is_empty(),
            Self::Content => !msg.content.is_empty(),
            Self::ToolCall => msg.has_tool_calls(),
        }
    }

    /// Whether this element contributes its keys to `msg`'s `--json` record.
    ///
    /// Same as [`Self::carried_by`] except for the text elements: a selected
    /// text element is a **key of the owning role**, emitted even when the
    /// text is empty (the projection always emits `content`, so consumers get
    /// a stable key set). A row is never *selected* off an empty text element,
    /// so this only widens records of rows selected by another element of the
    /// same filter.
    fn in_record(self, msg: &SessionMessage) -> bool {
        match self {
            Self::UserText | Self::Content => msg.role == self.owner_role(),
            _ => self.carried_by(msg),
        }
    }
}

/// A set of selected elements.
///
/// Iteration follows [`Element::ALL`] (declaration order), which is what keeps
/// body lines and `--json` keys in a stable, canonical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Selection(u8);

impl Selection {
    /// Nothing selected — selects no row at all (the CLI's `all` default makes
    /// this unreachable through `Filter::from_args`).
    const EMPTY: Self = Self(0);

    /// Every element — what `all` renders (its `--json` stays raw payloads).
    const ALL: Self = Self((1 << Element::ALL.len()) - 1);

    fn insert(&mut self, element: Element) {
        self.0 |= 1 << element.index();
    }

    fn insert_all(&mut self, elements: [Element; 3]) {
        for element in elements {
            self.insert(element);
        }
    }

    fn contains(self, element: Element) -> bool {
        self.0 & (1 << element.index()) != 0
    }

    /// The selected elements, in canonical order.
    fn iter(self) -> impl Iterator<Item = Element> {
        Element::ALL
            .into_iter()
            .filter(move |element| self.contains(*element))
    }

    /// Whether `msg` carries at least one selected element — the row-selection
    /// rule (a payload that does not decode carries no element: `all` only).
    fn carries_any(self, msg: &SessionMessage) -> bool {
        self.iter().any(|element| element.carried_by(msg))
    }
}

/// A parsed `--filter` value — the single source of truth for row selection,
/// text rendering and `--json` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    /// `all` (the default): unfiltered — every row (undecodable payloads
    /// included), every element, `--json` raw payloads verbatim.
    All,
    /// A union of elements.
    Elements(Selection),
}

impl Filter {
    /// Fold the command line's names into one filter.
    ///
    /// Comma-separated / repeated values are a **union** (`user,content` = the
    /// two text elements); `all` is the universe, so a list containing it is
    /// unfiltered (`all,user` ≡ `all`) — which is what a union with everything
    /// means. `assistant` expands to a message's own elements, so
    /// `assistant,tool_result` simply selects one more element.
    fn from_args(args: &[FilterArg]) -> Self {
        let mut selection = Selection::EMPTY;
        for arg in args {
            match arg {
                FilterArg::All => return Self::All,
                FilterArg::User => selection.insert(Element::UserText),
                FilterArg::Assistant => selection.insert_all(Element::ASSISTANT),
                FilterArg::Reasoning => selection.insert(Element::Reasoning),
                FilterArg::Content => selection.insert(Element::Content),
                FilterArg::ToolCall => selection.insert(Element::ToolCall),
                FilterArg::ToolResult => selection.insert(Element::ToolResult),
            }
        }
        Self::Elements(selection)
    }

    /// The elements this filter renders (`all` = every element).
    fn elements(self) -> Selection {
        match self {
            Self::All => Selection::ALL,
            Self::Elements(selection) => selection,
        }
    }

    /// Whether `msg` is selected: `all` takes everything, an element filter
    /// takes the messages carrying at least one selected element.
    fn matches(self, msg: &SessionMessage) -> bool {
        match self {
            Self::All => true,
            Self::Elements(selection) => selection.carries_any(msg),
        }
    }
}

impl fmt::Display for Filter {
    /// The canonical name: `all`, or the element names in canonical order
    /// (`content,user` prints as `user,content`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::Elements(selection) => {
                for (index, element) in selection.iter().enumerate() {
                    if index > 0 {
                        f.write_str(",")?;
                    }
                    f.write_str(element.name())?;
                }
                Ok(())
            }
        }
    }
}

/// Entry point for `wing tail`.
pub async fn run_tail(session_id: &str, n: usize, filter: &[FilterArg], json: bool) -> ExitCode {
    run_messages(
        session_id,
        n,
        Filter::from_args(filter),
        json,
        /* from_head = */ false,
    )
    .await
}

/// Entry point for `wing head`.
pub async fn run_head(session_id: &str, n: usize, filter: &[FilterArg], json: bool) -> ExitCode {
    run_messages(
        session_id,
        n,
        Filter::from_args(filter),
        json,
        /* from_head = */ true,
    )
    .await
}

/// One fetched history row: the raw payload — emitted verbatim by `--json`
/// under `all` — plus its typed view. Element filters build stripped element
/// records from `view` instead.
///
/// `view` is `None` when the payload is not a Message projection (non-object,
/// or any mirrored field with a mismatched type). Such rows carry no element,
/// so they match `all` only and print the `[unknown]` header without a body —
/// the same visible outcome the old sniffing produced for payloads carrying no
/// role (payloads with a role but a mistyped mirrored field used to print the
/// real role; the backend projection never produces them).
struct HistoryRow {
    raw: Value,
    view: Option<SessionMessage>,
}

impl HistoryRow {
    fn new(raw: Value) -> Self {
        let view = match SessionMessage::from_json(&raw) {
            Ok(view) => Some(view),
            Err(e) => {
                tracing::debug!(error = %e, "history payload is not a Message projection");
                None
            }
        };
        Self { raw, view }
    }
}

async fn run_messages(
    session_id: &str,
    n: usize,
    filter: Filter,
    json: bool,
    from_head: bool,
) -> ExitCode {
    match fetch_messages(session_id).await {
        Ok(messages) => {
            let rows: Vec<HistoryRow> = messages.into_iter().map(HistoryRow::new).collect();
            let selected = select_rows(&rows, filter, n, from_head);

            if json {
                common::print_json_compact(&json_payload(&selected, filter));
            } else {
                print_messages(&selected, filter);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let cmd = if from_head { "head" } else { "tail" };
            eprintln!("wing {cmd} error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn fetch_messages(session_id: &str) -> Result<Vec<Value>> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;

    // Try GET /api/session/get; if 404, resume the session first.
    match http.get_session(session_id).await {
        Ok(resp) => Ok(resp.messages),
        Err(e) => {
            // Check if it's a 404 (session not in memory).
            let err_str = e.to_string();
            if err_str.contains("404") || err_str.contains("not found") {
                // Try to resume the session from disk.
                http.resume_session(session_id).await?;
                let resp = http.get_session(session_id).await?;
                Ok(resp.messages)
            } else {
                Err(anyhow::anyhow!("{e}"))
            }
        }
    }
}

/// The rows to print: the filter's selection, then the first (`head`) or last
/// (`tail`) `n` of it.
fn select_rows(rows: &[HistoryRow], filter: Filter, n: usize, from_head: bool) -> Vec<&HistoryRow> {
    let selected = filter_messages(rows, filter);
    if from_head {
        selected.into_iter().take(n).collect()
    } else {
        let start = selected.len().saturating_sub(n);
        selected.into_iter().skip(start).collect()
    }
}

/// Filter rows by element.
fn filter_messages(rows: &[HistoryRow], filter: Filter) -> Vec<&HistoryRow> {
    rows.iter()
        .filter(|row| match &row.view {
            Some(msg) => filter.matches(msg),
            None => matches!(filter, Filter::All),
        })
        .collect()
}

/// Body lines of one message under a filter's element selection
/// (header/separator excluded).
///
/// The selected elements are walked in canonical order and each element the
/// message carries renders exactly once — so a section the caller did not
/// select cannot appear, and no section can appear twice. In particular a tool
/// message carries no assistant text: its `content` *is* the tool result, so
/// only the tool-result element renders it (rendering it as text as well
/// printed every tool result twice in the default `all` view).
fn message_body_lines(msg: &SessionMessage, selection: Selection) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    for element in selection.iter() {
        if !element.carried_by(msg) {
            continue;
        }
        match element {
            // The row's own text — user text / assistant text.
            Element::UserText | Element::Content => push_paragraph(&mut lines, &msg.content),
            Element::Reasoning => push_paragraph(&mut lines, reasoning(msg)),
            Element::ToolCall => {
                for tc in msg.tool_calls() {
                    // The call id prints so call and result (`← id`) lines can
                    // be matched. Absent `arguments` prints as `()`, explicit
                    // null as `(null)` — the distinction the typed mirror keeps
                    // on purpose.
                    let args = tc
                        .arguments
                        .as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    lines.push(String::new());
                    lines.push(format!("  → [{}] {}({args})", tc.id, tc.name));
                }
            }
            Element::ToolResult => {
                let tool_call_id = msg.tool_call_id.as_deref().unwrap_or("");
                lines.push(String::new());
                lines.push(format!("  ← {tool_call_id}"));
                // Truncate long tool results.
                let display = common::truncate_chars(&msg.content, 500);
                lines.push(format!("  {display}"));
            }
        }
    }

    lines
}

/// The message's reasoning text (`""` when absent).
fn reasoning(msg: &SessionMessage) -> &str {
    msg.reasoning_content.as_deref().unwrap_or("")
}

/// Append a blank separator + the paragraph; empty text renders nothing.
fn push_paragraph(lines: &mut Vec<String>, text: &str) {
    if text.is_empty() {
        return;
    }
    lines.push(String::new());
    lines.push(text.to_string());
}

/// The `--json` payload for the selected rows: stripped element records for an
/// element filter, raw payloads (verbatim) for `all`.
fn json_payload(selected: &[&HistoryRow], filter: Filter) -> Vec<Value> {
    match filter {
        Filter::All => selected.iter().map(|row| row.raw.clone()).collect(),
        Filter::Elements(selection) => selected
            .iter()
            .filter_map(|row| row.view.as_ref().map(|msg| element_record(msg, selection)))
            .collect(),
    }
}

/// One `--json` record for an element filter: the message stripped to the
/// selected element(s) — the machine counterpart of [`message_body_lines`]
/// (text and `--json` must filter alike; both walk the same selection through
/// the same element definitions).
///
/// Key stability: every record carries `uuid` (`null` when the payload has
/// none, matching the projection) and `content` whenever a text element is
/// selected (the projection always emits `content`). `reasoning_content` /
/// `tool_calls` / `tool_call_id` appear only when the message carries them.
fn element_record(msg: &SessionMessage, selection: Selection) -> Value {
    let mut record = Map::new();
    record.insert(
        "uuid".to_string(),
        match msg.uuid.clone() {
            Some(uuid) => Value::String(uuid),
            None => Value::Null,
        },
    );

    for element in selection.iter() {
        if !element.in_record(msg) {
            continue;
        }
        match element {
            Element::UserText | Element::Content => {
                record.insert("content".to_string(), Value::String(msg.content.clone()));
            }
            Element::Reasoning => {
                insert_non_empty(&mut record, "reasoning_content", reasoning(msg));
            }
            Element::ToolCall => {
                record.insert("tool_calls".to_string(), tool_calls_value(msg));
            }
            Element::ToolResult => {
                insert_non_empty(
                    &mut record,
                    "tool_call_id",
                    msg.tool_call_id.as_deref().unwrap_or(""),
                );
                record.insert("content".to_string(), Value::String(msg.content.clone()));
            }
        }
    }

    Value::Object(record)
}

fn insert_non_empty(record: &mut Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        record.insert(key.to_string(), Value::String(value.to_string()));
    }
}

/// The message's `tool_calls` in projection shape. `arguments` is always
/// present (`null` when absent): the mirror keeps absent apart from explicit
/// null for the text renderer, while the projection — and so the json — has
/// the single null form.
fn tool_calls_value(msg: &SessionMessage) -> Value {
    Value::Array(
        msg.tool_calls()
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "name": tc.name,
                    "arguments": tc.arguments.clone().unwrap_or(Value::Null),
                })
            })
            .collect(),
    )
}

/// Header line for one row: `[role] uuid`.
///
/// An empty/absent role and an undecodable payload both print `[unknown]`
/// (the old sniffing did the same).
fn header_line(row: &HistoryRow) -> String {
    let (role, uuid) = match &row.view {
        Some(msg) => (msg.role.as_str(), msg.uuid.as_deref().unwrap_or("")),
        None => ("unknown", ""),
    };
    let role = if role.is_empty() { "unknown" } else { role };
    format!("[{role}] {uuid}")
}

fn print_messages(rows: &[&HistoryRow], filter: Filter) {
    if rows.is_empty() {
        println!("No messages matching filter '{filter}'.");
        return;
    }

    let selection = filter.elements();
    for row in rows {
        println!("─────────────────────────────────────────────");
        println!("{}", header_line(row));
        if let Some(msg) = &row.view {
            for line in message_body_lines(msg, selection) {
                println!("{line}");
            }
        }
    }
    println!("─────────────────────────────────────────────");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Assistant message carrying reasoning + text + one tool call — the
    /// kitchen-sink case every element filter must slice correctly.
    fn assistant_msg() -> Value {
        json!({
            "role": "assistant",
            "uuid": "u1",
            "reasoning_content": "thinking hard",
            "content": "the answer",
            "tool_calls": [
                {"id": "tc_bash", "name": "bash", "arguments": {"cmd": "ls"}}
            ],
        })
    }

    fn tool_result_msg() -> Value {
        json!({
            "role": "tool",
            "uuid": "u2",
            "tool_call_id": "tc1",
            "content": "file-a\nfile-b",
        })
    }

    fn rows(values: Vec<Value>) -> Vec<HistoryRow> {
        values.into_iter().map(HistoryRow::new).collect()
    }

    fn decoded(value: Value) -> SessionMessage {
        SessionMessage::from_json(&value).expect("test payload must be a Message projection")
    }

    /// The CLI's own path into the filter: `--filter` splits on `,`
    /// (`value_delimiter = ','`) and every name goes through the clap
    /// vocabulary — so the tests exercise the shipped contract, and a name the
    /// tests use is a name the CLI accepts.
    fn filter(raw: &str) -> Filter {
        let args: Vec<FilterArg> = raw
            .split(',')
            .map(|name| {
                FilterArg::from_str(name, false).unwrap_or_else(|e| panic!("--filter {name}: {e}"))
            })
            .collect();
        Filter::from_args(&args)
    }

    fn selection(elements: &[Element]) -> Selection {
        let mut selection = Selection::EMPTY;
        for element in elements {
            selection.insert(*element);
        }
        selection
    }

    // ── the vocabulary: clap names ⇄ elements ────────

    #[test]
    fn filter_args_expand_to_the_documented_element_sets() {
        // Single values: exactly the table in the module docs.
        assert_eq!(filter("all"), Filter::All);
        assert_eq!(
            filter("user"),
            Filter::Elements(selection(&[Element::UserText]))
        );
        assert_eq!(
            filter("reasoning"),
            Filter::Elements(selection(&[Element::Reasoning]))
        );
        assert_eq!(
            filter("content"),
            Filter::Elements(selection(&[Element::Content]))
        );
        assert_eq!(
            filter("tool_call"),
            Filter::Elements(selection(&[Element::ToolCall]))
        );
        assert_eq!(
            filter("tool_result"),
            Filter::Elements(selection(&[Element::ToolResult]))
        );
        // `assistant` is not a separate rule: it is exactly the union of a
        // message's own elements.
        assert_eq!(filter("assistant"), filter("reasoning,content,tool_call"));
    }

    #[test]
    fn filter_args_combine_as_a_union_order_insensitively() {
        // The reported path: two names, one element set.
        assert_eq!(
            filter("user,content"),
            Filter::Elements(selection(&[Element::UserText, Element::Content]))
        );
        assert_eq!(filter("content,user"), filter("user,content"));
        // Repeats are idempotent (a union, not a multiset).
        assert_eq!(filter("user,user"), filter("user"));
        assert_eq!(
            filter("assistant,tool_result"),
            Filter::Elements(selection(&[
                Element::Reasoning,
                Element::Content,
                Element::ToolCall,
                Element::ToolResult,
            ]))
        );
        // The canonical name: declaration order, deduplicated.
        assert_eq!(filter("content,user").to_string(), "user,content");
        // The expansion is printed as selected — no name is re-collapsed, so
        // the message names exactly the elements the filter holds.
        assert_eq!(
            filter("tool_result,assistant").to_string(),
            "reasoning,content,tool_call,tool_result"
        );
        assert_eq!(filter("all").to_string(), "all");
    }

    #[test]
    fn filter_args_treat_all_as_the_universe() {
        // A union with everything is everything — including a later name.
        assert_eq!(filter("all,user"), Filter::All);
        assert_eq!(filter("user,all,content"), Filter::All);
    }

    #[test]
    fn unknown_filter_names_are_rejected_by_the_vocabulary() {
        // The old module matched unknown values with `_ => true`: `--filter
        // user,content` (and every typo) silently printed the unfiltered view.
        // The vocabulary is now closed — clap rejects anything else before the
        // request (see `tail_filter_e2e` for the process-level proof).
        for bogus in ["bogus", "", "user,", "*", "assistant_content"] {
            assert!(
                FilterArg::from_str(bogus, false).is_err(),
                "{bogus:?} must not parse"
            );
        }
    }

    // ── row selection ────────────────────────────────

    #[test]
    fn filter_by_role() {
        let msgs = rows(vec![
            assistant_msg(),
            json!({"role": "user", "content": "hi"}),
        ]);
        assert_eq!(filter_messages(&msgs, filter("user")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("assistant")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("all")).len(), 2);
    }

    #[test]
    fn filter_reasoning_requires_non_empty_assistant_reasoning() {
        let empty = json!({"role": "assistant", "reasoning_content": ""});
        let msgs = rows(vec![assistant_msg(), empty]);
        // Empty-string reasoning is not a reasoning message.
        assert_eq!(filter_messages(&msgs, filter("reasoning")).len(), 1);

        // Reasoning is an assistant message's element: `reasoning_content` on
        // another role is not reasoning to select (nor to render — that is the
        // same rule, read once).
        let foreign = json!({"role": "user", "content": "hi", "reasoning_content": "r"});
        let msgs = rows(vec![foreign.clone()]);
        assert!(filter_messages(&msgs, filter("reasoning")).is_empty());
        let msg = decoded(foreign);
        assert!(!Element::Reasoning.carried_by(&msg));
        assert!(
            message_body_lines(&msg, Selection::ALL)
                .join("\n")
                .contains("hi")
        );
    }

    #[test]
    fn filter_content_includes_tool_call_messages() {
        // Element-wise selection: carrying tool calls does not disqualify a
        // message from yielding its text — the old purity rule ("no
        // tool_calls") silently dropped every tool-call turn's narration.
        // Tool-only messages still pass no `content`: there is no text to
        // strip.
        let msgs = rows(vec![
            assistant_msg(),
            json!({"role": "assistant", "content": "plain"}),
            json!({
                "role": "assistant",
                "tool_calls": [{"id": "t", "name": "Bash"}]
            }),
        ]);
        assert_eq!(filter_messages(&msgs, filter("content")).len(), 2);
    }

    #[test]
    fn filter_tool_result_selects_tool_role() {
        let msgs = rows(vec![assistant_msg(), tool_result_msg()]);
        assert_eq!(filter_messages(&msgs, filter("tool_result")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("tool_call")).len(), 1);
    }

    #[test]
    fn filter_tool_call_requires_non_empty_array() {
        // Absent / null / [] are all "no tool call" — those messages yield
        // their text under "content" like any other text-bearing message.
        let empty = json!({"role": "assistant", "content": "x", "tool_calls": []});
        let null = json!({"role": "assistant", "content": "x", "tool_calls": null});
        let absent = json!({"role": "assistant", "content": "x"});
        let msgs = rows(vec![empty, null, absent, assistant_msg()]);
        assert_eq!(filter_messages(&msgs, filter("tool_call")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("content")).len(), 4);
    }

    #[test]
    fn undecodable_row_only_matches_all() {
        let msgs = rows(vec![
            json!({"invalid": "message"}),
            json!({"role": "user", "content": "hi"}),
            assistant_msg(),
            tool_result_msg(),
        ]);
        // Element filters keep selecting their real rows, never the undecodable
        // one (the old sniffing had no role to match either).
        assert_eq!(filter_messages(&msgs, filter("user")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("assistant")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("tool_call")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("tool_result")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("reasoning")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("content")).len(), 1);
        // Unions select the rows of their elements, still never that payload.
        assert_eq!(filter_messages(&msgs, filter("user,content")).len(), 2);
        assert_eq!(
            filter_messages(&msgs, filter("user,assistant,tool_result")).len(),
            3
        );
        // `all` is the only filter that keeps it.
        assert_eq!(filter_messages(&msgs, filter("all")).len(), 4);
    }

    #[test]
    fn mistyped_field_drops_row_from_named_filters() {
        // One mismatched mirrored field (here `uuid`) fails the whole decode:
        // the row leaves every element filter — even one unrelated to the bad
        // field (`--filter user`) — and prints without a body, so `--json`
        // selection shrinks with it. The old sniffing rendered the readable
        // fields instead. Out-of-contract payloads only: `serialize_message`
        // types every field, so the backend never produces this.
        let msgs = rows(vec![
            json!({"role": "user", "content": "hi", "uuid": 5}),
            json!({"role": "user", "content": "real"}),
        ]);
        assert_eq!(filter_messages(&msgs, filter("user")).len(), 1);
        assert_eq!(filter_messages(&msgs, filter("all")).len(), 2);
        assert_eq!(header_line(&msgs[0]), "[unknown] ");

        // The same holds for a feature element when the mistyped field is
        // unrelated to it: bad `tool_calls` kills a `--filter reasoning` match.
        let msgs = rows(vec![json!({
            "role": "assistant",
            "content": "x",
            "reasoning_content": "r",
            "tool_calls": "oops"
        })]);
        assert_eq!(filter_messages(&msgs, filter("reasoning")).len(), 0);
    }

    // ── head / tail slicing over the selection ───────

    #[test]
    fn select_rows_slices_the_filtered_run() {
        let corpus = rows(vec![
            json!({"role": "user", "uuid": "u1", "content": "one"}),
            assistant_msg(),
            json!({"role": "user", "uuid": "u2", "content": "two"}),
        ]);

        // head / tail take from the *selected* run — `user` skips the middle
        // assistant row for both.
        let head = select_rows(&corpus, filter("user"), 1, true);
        assert_eq!(header_line(head[0]), "[user] u1");
        let tail = select_rows(&corpus, filter("user"), 1, false);
        assert_eq!(header_line(tail[0]), "[user] u2");

        // A union keeps the order of the log, so the two endpoints differ the
        // same way (`user,content` also selects the assistant row).
        let head = select_rows(&corpus, filter("user,content"), 1, true);
        assert_eq!(header_line(head[0]), "[user] u1");
        let tail = select_rows(&corpus, filter("user,content"), 1, false);
        assert_eq!(header_line(tail[0]), "[user] u2");

        // n over the selection is clamped, not padded — and a selection with
        // no match stays empty from both ends.
        assert_eq!(select_rows(&corpus, filter("user"), 99, true).len(), 2);
        assert_eq!(select_rows(&corpus, filter("user"), 0, false).len(), 0);
        assert!(select_rows(&corpus, filter("tool_result"), 10, true).is_empty());
    }

    // ── the matrix: no unselected section ever leaks ──

    /// One row per element, each carrying a unique sentinel — the whole
    /// vocabulary in one corpus.
    fn corpus() -> Vec<HistoryRow> {
        rows(vec![
            json!({"role": "user", "uuid": "u-user", "content": "USER-TEXT"}),
            json!({
                "role": "assistant",
                "uuid": "u-asst",
                "reasoning_content": "REASONING",
                "content": "ANSWER",
                "tool_calls": [{"id": "call-1", "name": "bash", "arguments": {"cmd": "ls"}}],
            }),
            json!({
                "role": "tool",
                "uuid": "u-tool",
                "tool_call_id": "call-1",
                "content": "RESULT-BODY",
            }),
        ])
    }

    /// One sentinel per element, in `Element::ALL` order — text and `--json`
    /// spellings. Text rendering and json records must agree on all five:
    /// whatever the filter did not select is absent from **both**.
    const SENTINELS: [(&str, &str); 5] = [
        ("USER-TEXT", r#""content":"USER-TEXT""#),
        ("REASONING", r#""reasoning_content":"REASONING""#),
        ("ANSWER", r#""content":"ANSWER""#),
        ("→ [call-1]", r#""tool_calls":[{"id":"call-1""#),
        ("RESULT-BODY", r#""content":"RESULT-BODY""#),
    ];

    /// Every rendered line of the selected rows (headers included — they are
    /// part of what the command prints).
    fn render(rows: &[&HistoryRow], selection: Selection) -> String {
        rows.iter()
            .flat_map(|row| {
                std::iter::once(header_line(row)).chain(match &row.view {
                    Some(msg) => message_body_lines(msg, selection),
                    None => Vec::new(),
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn filter_matrix_selects_exactly_the_named_elements() {
        // (filter, selected rows, elements present) — single values, unions,
        // and `assistant` as a union all in one table. Commas and the shapes
        // the vocabulary accepts are exercised through `filter()`.
        let cases: &[(&str, usize, [bool; 5])] = &[
            ("all", 3, [true, true, true, true, true]),
            ("user", 1, [true, false, false, false, false]),
            ("assistant", 1, [false, true, true, true, false]),
            ("reasoning", 1, [false, true, false, false, false]),
            ("content", 1, [false, false, true, false, false]),
            ("tool_call", 1, [false, false, false, true, false]),
            ("tool_result", 1, [false, false, false, false, true]),
            // The reported path.
            ("user,content", 2, [true, false, true, false, false]),
            ("assistant,tool_result", 2, [false, true, true, true, true]),
            ("content,tool_result", 2, [false, false, true, false, true]),
            ("user,reasoning", 2, [true, true, false, false, false]),
            ("user,tool_result", 2, [true, false, false, false, true]),
            (
                "tool_call,tool_result",
                2,
                [false, false, false, true, true],
            ),
            ("reasoning,content", 1, [false, true, true, false, false]),
            // Union of every element — same rows and sections as `all`, only
            // the `--json` shape differs (`all` keeps raw payloads).
            (
                "user,assistant,tool_result",
                3,
                [true, true, true, true, true],
            ),
        ];

        for (raw, count, present) in cases {
            let rows = corpus();
            let parsed = filter(raw);
            let selected = filter_messages(&rows, parsed);
            assert_eq!(selected.len(), *count, "{raw}: selected rows");

            let text = render(&selected, parsed.elements());
            let json = serde_json::to_string(&json_payload(&selected, parsed)).unwrap();
            for (index, (text_sentinel, json_sentinel)) in SENTINELS.iter().enumerate() {
                let want = present[index];
                assert_eq!(
                    want,
                    text.contains(text_sentinel),
                    "{raw}: text {text_sentinel:?} presence\n{text}"
                );
                assert_eq!(
                    want,
                    json.contains(json_sentinel),
                    "{raw}: json {json_sentinel:?} presence\n{json}"
                );
            }
        }
    }

    #[test]
    fn user_content_never_leaks_another_section() {
        // Regression, user-reported (`20261010-074437-440e10fb`): `--filter
        // user,content` matched no arm, so `matches_filter`'s `_ => true`
        // selected every row and the section filter fell back to `All` — the
        // output was the *unfiltered* view (reasoning, tool calls, tool
        // results), i.e. a silent degrade.
        let rows = corpus();
        let parsed = filter("user,content");
        let selected = filter_messages(&rows, parsed);
        assert_eq!(selected.len(), 2, "只该剩 user 文本与 assistant 文本两条");

        let text = render(&selected, parsed.elements());
        assert!(text.contains("USER-TEXT"), "{text}");
        assert!(text.contains("ANSWER"), "{text}");
        for leaked in ["REASONING", "→ [call-1]", "← call-1", "RESULT-BODY"] {
            assert!(!text.contains(leaked), "{leaked} leaked:\n{text}");
        }
        // Same selection in json: two stripped text records, nothing else.
        assert_eq!(
            json_payload(&selected, parsed),
            vec![
                json!({"uuid": "u-user", "content": "USER-TEXT"}),
                json!({"uuid": "u-asst", "content": "ANSWER"}),
            ]
        );
    }

    #[test]
    fn text_and_json_agree_on_the_selected_elements() {
        // The core contract, once per value of the vocabulary: one filter
        // yields the same element set in text and in `--json`. Sentinels make
        // element presence checkable on both sides; the `user`-side cases pin
        // the foreign-section rule (a user message's reasoning must leak
        // nowhere).
        let cases: &[(&str, Value, &[&str])] = &[
            ("content", assistant_msg(), &["the answer"]),
            ("reasoning", assistant_msg(), &["thinking hard"]),
            ("tool_call", assistant_msg(), &["tc_bash"]),
            (
                "assistant",
                assistant_msg(),
                &["the answer", "thinking hard", "tc_bash"],
            ),
            ("tool_result", tool_result_msg(), &["tc1", "file-a"]),
            (
                "user",
                json!({
                    "role": "user",
                    "uuid": "u5",
                    "content": "user text",
                    "reasoning_content": "user-side reasoning"
                }),
                &["user text"],
            ),
            // A union: the two text elements, nothing else.
            (
                "user,content",
                json!({
                    "role": "user",
                    "uuid": "u5",
                    "content": "user text",
                    "reasoning_content": "user-side reasoning"
                }),
                &["user text"],
            ),
        ];
        let sentinels = [
            "the answer",
            "thinking hard",
            "tc_bash",
            "tc1",
            "file-a",
            "user text",
            "user-side reasoning",
        ];
        for (raw, value, expected) in cases {
            let parsed = filter(raw);
            let msg = decoded(value.clone());
            let text = message_body_lines(&msg, parsed.elements()).join("\n");
            let record = element_record(&msg, parsed.elements()).to_string();
            assert!(!record.contains("\"role\""), "{raw}: {record}");
            for sentinel in sentinels {
                let want = expected.contains(&sentinel);
                assert_eq!(
                    want,
                    text.contains(sentinel),
                    "{raw}/{sentinel} in text:\n{text}"
                );
                assert_eq!(
                    want,
                    record.contains(sentinel),
                    "{raw}/{sentinel} in json:\n{record}"
                );
            }
        }
    }

    // ── message_body_lines: elements slice sections ──

    #[test]
    fn body_lines_content_does_not_leak_reasoning() {
        // Regression: `--filter content` used to print reasoning too, because
        // the filter only selected messages while print_messages rendered
        // every section.
        let lines = message_body_lines(&decoded(assistant_msg()), filter("content").elements());
        let joined = lines.join("\n");
        assert!(joined.contains("the answer"));
        assert!(!joined.contains("thinking hard"));
        assert!(!joined.contains("bash"));
    }

    #[test]
    fn body_lines_reasoning_only() {
        let lines = message_body_lines(&decoded(assistant_msg()), filter("reasoning").elements());
        let joined = lines.join("\n");
        assert!(joined.contains("thinking hard"));
        assert!(!joined.contains("the answer"));
        assert!(!joined.contains("bash"));
    }

    #[test]
    fn body_lines_tool_call_only() {
        let lines = message_body_lines(&decoded(assistant_msg()), filter("tool_call").elements());
        let joined = lines.join("\n");
        assert!(joined.contains("→ [tc_bash] bash"));
        assert!(!joined.contains("thinking hard"));
        assert!(!joined.contains("the answer"));
    }

    #[test]
    fn body_lines_tool_result_only() {
        let lines = message_body_lines(
            &decoded(tool_result_msg()),
            filter("tool_result").elements(),
        );
        let joined = lines.join("\n");
        assert!(joined.contains("← tc1"));
        assert!(joined.contains("file-a"));
    }

    #[test]
    fn body_lines_all_renders_tool_result_once() {
        // Regression: the default `all` view printed a tool message's content
        // twice — once as the text section (full copy) and once as the tool
        // result. A tool message has no assistant text: its content *is* the
        // result, so the result element is its only renderer.
        let msg = decoded(tool_result_msg());
        let all = message_body_lines(&msg, Selection::ALL);
        let joined = all.join("\n");
        assert_eq!(
            joined.matches("file-a").count(),
            1,
            "tool result must render exactly once: {joined}"
        );
        assert_eq!(joined.matches("file-b").count(), 1, "{joined}");
        assert!(joined.contains("← tc1"), "{joined}");
        // Byte-identical to the dedicated `--filter tool_result` view: one
        // rendering definition per element, no extra copy smuggled into `all`.
        assert_eq!(
            all,
            message_body_lines(&msg, filter("tool_result").elements())
        );
    }

    #[test]
    fn body_lines_all_truncates_long_tool_result_once() {
        // `all` reuses the tool-result rendering verbatim — header/separator
        // excluded, a tool message is its result element and nothing else
        // (hence exactly the 3 result lines: blank, `← id`, truncated body).
        // Pinning the whole body — not just a copy count — is the point: a
        // second renderer would necessarily add lines. `--json` carries the
        // stored content in full (the module docs carry the caps).
        let long = "x".repeat(600);
        let msg = decoded(json!({
            "role": "tool",
            "uuid": "u3",
            "tool_call_id": "tc1",
            "content": long,
        }));
        let lines = message_body_lines(&msg, Selection::ALL);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(!lines.join("\n").contains(&long), "no untruncated copy");
        assert!(lines[2].ends_with("..."), "{:?}", lines[2]);
    }

    #[test]
    fn body_lines_tool_result_is_never_routed_through_the_text_element() {
        // Element × tool-message matrix: a tool message's `content` is its
        // result, so only the result element renders it — never the text
        // element (that is what used to double-print it under `all`). The
        // regression is invisible under `--filter content` because that filter
        // selects assistant messages and so never reaches a tool row.
        let msg = decoded(tool_result_msg());
        for element in Element::ALL {
            let joined = message_body_lines(&msg, selection(&[element])).join("\n");
            let renders_result = matches!(element, Element::ToolResult);
            assert_eq!(
                joined.matches("file-a").count(),
                usize::from(renders_result),
                "{element:?} rendered the wrong number of copies: {joined}"
            );
        }
    }

    #[test]
    fn body_lines_all_renders_every_section() {
        let lines = message_body_lines(&decoded(assistant_msg()), Selection::ALL);
        let joined = lines.join("\n");
        assert!(joined.contains("thinking hard"));
        assert!(joined.contains("the answer"));
        assert!(joined.contains("→ [tc_bash] bash"));
    }

    #[test]
    fn body_lines_tool_args_absent_vs_null() {
        // Text keeps the sniffing-era split: absent `arguments` prints `()`,
        // explicit null prints `(null)`.
        let absent = decoded(json!({
            "role": "assistant",
            "tool_calls": [{"id": "a", "name": "Bash"}]
        }));
        let null = decoded(json!({
            "role": "assistant",
            "tool_calls": [{"id": "a", "name": "Bash", "arguments": null}]
        }));
        let lines = |msg: &SessionMessage| {
            message_body_lines(msg, filter("tool_call").elements()).join("\n")
        };
        assert!(
            lines(&absent).contains("→ [a] Bash()"),
            "{}",
            lines(&absent)
        );
        assert!(
            lines(&null).contains("→ [a] Bash(null)"),
            "{}",
            lines(&null)
        );
        // Json carries the projection's single null form for both.
        assert_eq!(
            element_record(&absent, filter("tool_call").elements())["tool_calls"][0]["arguments"],
            json!(null)
        );
        assert_eq!(
            element_record(&null, filter("tool_call").elements())["tool_calls"][0]["arguments"],
            json!(null)
        );
    }

    #[test]
    fn missing_fields_default() {
        // Only a role: no content / reasoning / tool_calls — decodes cleanly,
        // renders no body.
        let msg = decoded(json!({"role": "assistant"}));
        assert_eq!(msg.content, "");
        assert!(!msg.has_tool_calls());
        assert!(message_body_lines(&msg, Selection::ALL).is_empty());
    }

    // ── element_record / json_payload: machine output filters too ──

    #[test]
    fn element_record_strips_to_the_selected_element() {
        let msg = decoded(assistant_msg());
        assert_eq!(
            element_record(&msg, filter("content").elements()),
            json!({"uuid": "u1", "content": "the answer"})
        );
        assert_eq!(
            element_record(&msg, filter("reasoning").elements()),
            json!({"uuid": "u1", "reasoning_content": "thinking hard"})
        );
        assert_eq!(
            element_record(&msg, filter("tool_call").elements()),
            json!({
                "uuid": "u1",
                "tool_calls": [{"id": "tc_bash", "name": "bash", "arguments": {"cmd": "ls"}}]
            })
        );
        // `assistant` keeps the message's own elements — no foreign sections.
        assert_eq!(
            element_record(&msg, filter("assistant").elements()),
            json!({
                "uuid": "u1",
                "reasoning_content": "thinking hard",
                "content": "the answer",
                "tool_calls": [{"id": "tc_bash", "name": "bash", "arguments": {"cmd": "ls"}}]
            })
        );
        // A union merges the selected elements of the row, once each.
        assert_eq!(
            element_record(&msg, filter("reasoning,content").elements()),
            json!({
                "uuid": "u1",
                "reasoning_content": "thinking hard",
                "content": "the answer"
            })
        );
    }

    #[test]
    fn element_record_for_tool_and_user_rows() {
        assert_eq!(
            element_record(
                &decoded(tool_result_msg()),
                filter("tool_result").elements()
            ),
            json!({
                "uuid": "u2",
                "tool_call_id": "tc1",
                "content": "file-a\nfile-b"
            })
        );
        assert_eq!(
            element_record(
                &decoded(json!({"role": "user", "uuid": "u3", "content": "hi"})),
                filter("user").elements()
            ),
            json!({"uuid": "u3", "content": "hi"})
        );
    }

    #[test]
    fn element_record_keeps_uuid_and_content_keys_stable() {
        // `uuid` is always present (`null` when the payload has none) and
        // text elements always carry `content` (even empty) — projection
        // style; other element fields appear only when carried.
        let only_calls = decoded(json!({
            "role": "assistant",
            "uuid": "u4",
            "tool_calls": [{"id": "t", "name": "Bash"}]
        }));
        assert_eq!(
            element_record(&only_calls, filter("reasoning").elements()),
            json!({"uuid": "u4"})
        );
        // Absent `arguments` is the projection's null form in json (the
        // `()` / `(null)` split is text-renderer-only).
        assert_eq!(
            element_record(&only_calls, filter("assistant").elements()),
            json!({
                "uuid": "u4",
                "content": "",
                "tool_calls": [{"id": "t", "name": "Bash", "arguments": null}]
            })
        );
        // An empty tool result keeps its `content` key ("" — not omitted).
        let empty_result = decoded(json!({
            "role": "tool",
            "uuid": "t2",
            "tool_call_id": "call_2",
            "content": ""
        }));
        assert_eq!(
            element_record(&empty_result, filter("tool_result").elements()),
            json!({"uuid": "t2", "tool_call_id": "call_2", "content": ""})
        );
        // uuid absent → null (matching the raw payload), not "".
        let no_uuid = decoded(json!({"role": "user", "content": "hi"}));
        assert_eq!(
            element_record(&no_uuid, filter("user").elements()),
            json!({"uuid": null, "content": "hi"})
        );
    }

    #[test]
    fn json_payload_strips_named_filters_and_keeps_all_verbatim() {
        let msgs = rows(vec![
            assistant_msg(),
            json!({"role": "user", "uuid": "u2", "content": "hi"}),
        ]);
        // Element filters: stripped records — no `role`, no foreign sections.
        assert_eq!(
            json_payload(
                &filter_messages(&msgs, filter("content")),
                filter("content")
            ),
            vec![json!({"uuid": "u1", "content": "the answer"})]
        );
        assert_eq!(
            json_payload(&filter_messages(&msgs, filter("user")), filter("user")),
            vec![json!({"uuid": "u2", "content": "hi"})]
        );
        // A union strips too — never the raw payloads of the selected rows.
        assert_eq!(
            json_payload(
                &filter_messages(&msgs, filter("user,content")),
                filter("user,content")
            ),
            vec![
                json!({"uuid": "u1", "content": "the answer"}),
                json!({"uuid": "u2", "content": "hi"}),
            ]
        );
        // `all`: raw payloads, untouched.
        let all = json_payload(&filter_messages(&msgs, filter("all")), filter("all"));
        assert_eq!(all[0], assistant_msg());
        assert_eq!(
            all[1],
            json!({"role": "user", "uuid": "u2", "content": "hi"})
        );
    }

    // ── header + raw payload stability ──────────────

    #[test]
    fn header_line_fallbacks() {
        // Missing role (but decodable) → [unknown], like the sniffing era.
        let no_role = rows(vec![json!({"content": "x"})]);
        assert_eq!(header_line(&no_role[0]), "[unknown] ");

        // Undecodable payload → [unknown] header as well.
        let undecodable = rows(vec![json!({"invalid": "message"})]);
        assert_eq!(header_line(&undecodable[0]), "[unknown] ");

        // Normal case keeps role + uuid.
        let normal = rows(vec![assistant_msg()]);
        assert_eq!(header_line(&normal[0]), "[assistant] u1");
        // uuid absent → empty (no "null").
        let no_uuid = rows(vec![json!({"role": "user", "content": "x"})]);
        assert_eq!(header_line(&no_uuid[0]), "[user] ");
    }

    #[test]
    fn raw_payload_is_kept_verbatim_for_json() {
        // `--json` under `all` prints the raw payloads, so unknown fields
        // survive and the serialized array matches the fetched one byte for
        // byte (element filters build stripped records instead — see above).
        let fetched = vec![
            json!({"role": "assistant", "content": "x", "usage": {"in": 1}}),
            json!({"role": "user", "content": "y"}),
        ];
        let fetched_json = serde_json::to_string(&fetched).unwrap();

        let msgs = rows(fetched);
        let selected = filter_messages(&msgs, filter("all"));
        let raw = json_payload(&selected, filter("all"));
        assert_eq!(serde_json::to_string(&raw).unwrap(), fetched_json);
        assert!(serde_json::to_string(&raw).unwrap().contains("\"usage\""));
    }
}
