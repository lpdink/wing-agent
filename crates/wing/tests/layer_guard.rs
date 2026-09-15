//! Layering guard: the neutral layer stays neutral, and the UI never reaches up
//! into the App.
//!
//! The crate is layered `ui → shared → (render / protocol / config / util)`,
//! with `app → ui + shared`: the UI renders what the App orchestrates, so a
//! `ui → app` reference — or a `shared → app / ui / cmd / stdio / gateway / tui`
//! one — is a dependency cycle by construction. The neutral layer must not
//! reach for the render library either: the panels are pure state, rendering
//! lives in `ui`.
//!
//! Rust cannot express "this module must not name that one" for modules inside
//! a single crate (`pub(crate)` / private modules only narrow visibility, they
//! cannot forbid a direction), and a custom lint would add a toolchain
//! dependency — so the rule is pinned here, by scanning the real source tree.
//! `cargo test` runs this on every build.
//!
//! The scan is **textual and whitespace-insensitive**: every whitespace
//! character is dropped before matching, so `crate :: app`, `crate::{ app` and
//! a path split across lines are all seen as the single path they are; the
//! reported line is the original one the path starts on. Comments count too — a
//! reverse dependency must not exist in any form, and a stale doc pointer is
//! exactly how the next code pointer gets written.
//!
//! Per side the needles are `crate::NAME`, `crate::{NAME` (a nested import's
//! first member) and `super::NAME` (substring matching covers the whole
//! `super::super::NAME` chain); the neutral layer additionally forbids the path
//! form of the render library.
//!
//! **Outside the coverage by design**: a forbidden name that is *not* the first
//! member of a `crate::{…}` import group (`use crate::{ui, app};`), and
//! whatever macro expansion or `include!` produces — both need a Rust parser
//! rather than a text scan, and both are absent from this tree.
//!
//! There are no exemptions (`#[allow]`-style skips, whitelists and env switches
//! are all absent): the rule is "zero occurrences" on both sides, so the only
//! fix is to reference the neutral layer instead.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// Crate source root (`crates/wing/src`).
const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// One forbidden path occurrence: 1-based line number + the line itself.
#[derive(Debug, PartialEq, Eq)]
struct Offense {
    line: usize,
    text: String,
}

/// Source file under `src/`: path relative to `src` + its text.
#[derive(Debug)]
struct Source {
    path: String,
    text: String,
}

/// Every `.rs` file under `src/<layer>`, recursively (relative paths).
fn layer_sources(layer: &str) -> Vec<Source> {
    let root = PathBuf::from(SRC).join(layer);
    assert!(
        root.is_dir(),
        "layer `{layer}` is missing at {} — the guard must scan the real tree",
        root.display()
    );
    let mut sources = Vec::new();
    collect(&root, &mut sources);
    assert!(
        !sources.is_empty(),
        "layer `{layer}` holds no Rust sources — the guard would pass vacuously"
    );
    sources
}

/// Depth-first walk collecting `*.rs` files (sorted for stable reports).
fn collect(dir: &Path, out: &mut Vec<Source>) {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            out.push(Source {
                path: relative(&path),
                text,
            });
        }
    }
}

/// `src`-relative, slash-separated path (e.g. `ui/cells/ask_msg.rs`).
fn relative(path: &Path) -> String {
    path.strip_prefix(SRC)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ── The scanner ─────────────────────────────────────────────────

/// The source with every whitespace character dropped, remembering where each
/// kept character came from. Rust's token stream does not care about whitespace
/// inside a path (`crate :: app`, or `::app` on the next line), so neither
/// should the guard — while the report still names a real line of the real
/// file, and the boundary test still reads the real characters (squashing
/// merges `use` and `crate`, so it must not judge identifier boundaries).
struct Squashed {
    text: String,
    /// `(squashed byte offset, original byte offset, 1-based line)` per kept
    /// character.
    at: Vec<(usize, usize, usize)>,
}

impl Squashed {
    fn new(source: &str) -> Self {
        let mut text = String::with_capacity(source.len());
        let mut at = Vec::with_capacity(source.len());
        let mut line = 1;
        for (offset, ch) in source.char_indices() {
            if ch == '\n' {
                line += 1;
                continue;
            }
            if ch.is_whitespace() {
                continue;
            }
            at.push((text.len(), offset, line));
            text.push(ch);
        }
        Self { text, at }
    }

    /// `(original byte offset, 1-based line)` of the character at squashed byte
    /// `offset`.
    fn origin(&self, offset: usize) -> (usize, usize) {
        match self
            .at
            .binary_search_by_key(&offset, |(squashed, _, _)| *squashed)
        {
            Ok(i) => (self.at[i].1, self.at[i].2),
            // A needle always starts (and ends) on kept characters.
            Err(i) => self
                .at
                .get(i.saturating_sub(1))
                .map_or((0, 1), |&(_, o, l)| (o, l)),
        }
    }
}

/// Do the original characters around `start..end` allow the match to be a whole
/// path? A needle must not be part of a longer name on the sides where it ends
/// in an identifier character — `crate::app_ui` is a different module, and
/// `mycrate::app` is not the crate's `app`; a needle ending in `::` (`ratatui::`)
/// expects an identifier after it.
fn is_whole_path(source: &str, start: usize, end: usize, needle: &str) -> bool {
    fn ident(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }
    let head = !needle.chars().next().is_some_and(ident)
        || source[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !ident(c));
    let tail = !needle.chars().next_back().is_some_and(ident)
        || source[end..].chars().next().is_none_or(|c| !ident(c));
    head && tail
}

/// Lines of `source` mentioning `needle` as a path, whitespace-insensitive and
/// comments included. A line is reported once, however many hits it holds.
fn find_offenses(source: &str, needle: &str) -> Vec<Offense> {
    let squashed = Squashed::new(source);
    let original: Vec<&str> = source.lines().collect();
    let mut offenses: Vec<Offense> = Vec::new();
    let mut from = 0;
    while let Some(offset) = squashed.text[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let (first, line) = squashed.origin(start);
        let (last, _) = squashed.origin(end - 1);
        // ASCII needles: the byte after the last matched character is `last + 1`.
        if is_whole_path(source, first, last + 1, needle) {
            if !offenses.iter().any(|o| o.line == line) {
                offenses.push(Offense {
                    line,
                    text: original.get(line - 1).unwrap_or(&"").trim().to_string(),
                });
            }
        }
        // The needle is ASCII, so one byte in is a character boundary.
        from = start + 1;
    }
    offenses
}

// ── The rules ───────────────────────────────────────────────────

/// Spellings of a **crate-internal** module reference: the plain path, the
/// first member of a nested import, and the parent-relative form.
fn layer_needles(layers: &[&str]) -> Vec<String> {
    let mut needles = Vec::new();
    for name in layers {
        needles.push(format!("crate::{name}"));
        needles.push(format!("crate::{{{name}"));
        needles.push(format!("super::{name}"));
    }
    needles
}

/// Spellings of an **external crate** reference: its path form.
fn crate_needles(crates: &[&str]) -> Vec<String> {
    crates.iter().map(|name| format!("{name}::")).collect()
}

/// `ui/` renders what the App orchestrates: it must never name the App layer.
const UI_FORBIDDEN_LAYERS: &[&str] = &["app"];

/// The neutral layer must not name any layer above it, nor the render library
/// (panels are pure state; rendering belongs to `ui`).
const SHARED_FORBIDDEN_LAYERS: &[&str] = &["app", "ui", "cmd", "stdio", "gateway", "tui"];
const SHARED_FORBIDDEN_CRATES: &[&str] = &["ratatui"];

/// Files that used to reach up — the guard must actually see them.
const UI_ANCHORS: &[&str] = &["ui/panel.rs", "ui/chat_view/cell.rs", "ui/cells/ask_msg.rs"];

/// The neutral layer's files — the guard must actually see them.
const SHARED_ANCHORS: &[&str] = &[
    "shared/mod.rs",
    "shared/constants.rs",
    "shared/goal_role.rs",
    "shared/panels/mod.rs",
    "shared/panels/ask.rs",
    "shared/panels/picker.rs",
];

/// Scan `sources` for `needles`, panicking with every offending site.
fn assert_clean(layer: &str, sources: &[Source], needles: &[String]) {
    let mut report = String::new();
    for source in sources {
        for needle in needles {
            for offense in find_offenses(&source.text, needle) {
                report.push_str(&format!(
                    "\n  {}:{}  `{}`  →  {}",
                    source.path, offense.line, needle, offense.text
                ));
            }
        }
    }
    assert!(
        report.is_empty(),
        "layering violation under src/{layer}/ (dependency direction is \
         ui → shared, app → ui + shared):{report}\n\
         Reference the neutral layer (`crate::shared::…`) instead."
    );
}

/// The scan must reach these files, otherwise the rules below pass vacuously.
fn assert_scanned(sources: &[Source], anchors: &[&str]) {
    let seen: Vec<&str> = sources.iter().map(|s| s.path.as_str()).collect();
    for anchor in anchors {
        assert!(
            seen.contains(anchor),
            "guard is not scanning {anchor} (walk stopped early?)"
        );
    }
}

#[test]
fn ui_never_reaches_up_into_the_app_layer() {
    let sources = layer_sources("ui");
    assert_scanned(&sources, UI_ANCHORS);
    assert_clean("ui", &sources, &layer_needles(UI_FORBIDDEN_LAYERS));
}

#[test]
fn the_neutral_layer_never_reaches_up() {
    let sources = layer_sources("shared");
    assert_scanned(&sources, SHARED_ANCHORS);
    let mut needles = layer_needles(SHARED_FORBIDDEN_LAYERS);
    needles.extend(crate_needles(SHARED_FORBIDDEN_CRATES));
    assert_clean("shared", &sources, &needles);
}

// ── The scanner itself (synthetic inputs) ───────────────────────

/// Any needle hitting `source`?
fn hits(needles: &[String], source: &str) -> bool {
    needles.iter().any(|n| !find_offenses(source, n).is_empty())
}

#[test]
fn scanner_catches_every_spelling_of_a_reverse_dependency() {
    let needles = layer_needles(&["app"]);
    let spellings = [
        // The plain forms.
        "use crate::app::App;\n",
        "use crate::app;\n",
        "fn cell() -> Role {\n    Role::from(crate::app::goal::GoalRole::Executor)\n}\n",
        // Nested import groups.
        "use crate::{app::App};\n",
        "use crate::{\n    app::constants::TOOL_BASH,\n};\n",
        // Whitespace inside the path is not significant to Rust.
        "use crate :: app :: App;\n",
        "use crate\n    ::app::App;\n",
        // Parent-relative spellings, including the chained form.
        "use super::app::App;\n",
        "use super::super::app::App;\n",
        // Doc comments are references too.
        "//! the snapshot mirrors `crate :: app::goal::GoalRole`\n",
    ];
    for spelling in spellings {
        assert!(hits(&needles, spelling), "not caught: {spelling}");
    }
}

#[test]
fn scanner_catches_an_external_crate_path() {
    let needles = crate_needles(&["ratatui"]);
    for spelling in [
        "use ratatui::style::Style;\n",
        "use ratatui :: Style;\n",
        "Style::default().fg(palette.text) // ratatui::style relies on it\n",
    ] {
        assert!(hits(&needles, spelling), "not caught: {spelling}");
    }
    // Naming the crate in prose is not a reference — only its path form is.
    assert!(!hits(&needles, "//! never reaches for ratatui\n"));
}

#[test]
fn scanner_reports_the_original_line() {
    // Single line: the line and its text.
    let inline = "fn f() {\n    let r = crate::app::goal::GoalRole::Executor;\n}\n";
    assert_eq!(
        find_offenses(inline, "crate::app"),
        vec![Offense {
            line: 2,
            text: "let r = crate::app::goal::GoalRole::Executor;".into(),
        }]
    );
    // A path split across lines is reported at the line it starts on.
    let split = "line one\nuse crate\n    ::app::App;\nline four\n";
    assert_eq!(
        find_offenses(split, "crate::app"),
        vec![Offense {
            line: 2,
            text: "use crate".into(),
        }]
    );
    // A line is reported once, however many hits it holds.
    let twice = "use crate::app::x; use crate::app::y;\n";
    assert_eq!(find_offenses(twice, "crate::app").len(), 1);
    let doc = "//! State lives in `crate::app::ask_panel::AskPanel`.\n";
    assert_eq!(find_offenses(doc, "crate::app").len(), 1);
}

#[test]
fn scanner_ignores_lookalikes() {
    let benign = [
        "use crate::shared::panels::ask::AskPanel;",
        "use crate::app_ui::Thing;", // same prefix, different path segment
        "use crate::application::Launch;",
        "// the app keeps the live panel", // the word alone is not a reference
        "// a struct field named app",     // nor is an identifier
        "use crate::protocol::AskQuestion;",
    ]
    .join("\n");
    for needle in layer_needles(&["app", "ui", "gateway"]) {
        assert_eq!(find_offenses(&benign, &needle), Vec::new(), "{needle}");
    }
    // The same text under a needle it does contain.
    let found = find_offenses(&benign, "crate::shared");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line, 1);
    // …and a longer name is not a path either, whichever side the extra
    // characters are on.
    for lookalike in ["use mycrate::app::App;", "use crate::app_ui::Thing;"] {
        for needle in layer_needles(&["app"]) {
            assert_eq!(
                find_offenses(lookalike, &needle),
                Vec::new(),
                "{needle} in {lookalike}"
            );
        }
    }
}
