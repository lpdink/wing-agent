//! Layering guard: the neutral layer stays neutral, and the UI never reaches up
//! into the App.
//!
//! The crate is layered `ui → shared → (render / protocol / config / util)`,
//! with `app → ui + shared`: the UI renders what the App orchestrates, so a
//! `ui → app` reference (or a `shared → app` / `shared → ui` reference) is a
//! dependency cycle by construction.
//!
//! Rust cannot express "this module must not name that one" for modules inside
//! a single crate (`pub(crate)` / private modules only narrow visibility, they
//! cannot forbid a direction), and a custom lint would add a toolchain
//! dependency — so the rule is pinned here, by scanning the real source tree.
//! `cargo test` runs this on every build.
//!
//! The scan is deliberately **textual**: a reverse dependency must not exist in
//! any form, including a doc comment pointing at it — a stale doc pointer is
//! exactly how the next code pointer gets written. There are no exemptions
//! (`#[allow]`-style skips, whitelists and env switches are all absent), because
//! the rule is "zero occurrences": the only fix is to reference the neutral
//! layer instead.

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

/// Does `line` contain `needle` (a path such as `crate::app`) as a **path
/// segment**? A longer identifier with the same prefix (`crate::app_ui`) is not
/// the app layer and must not trip the guard.
fn mentions(line: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = line[from..].find(needle) {
        let end = from + offset + needle.len();
        match line[end..].chars().next() {
            Some(c) if c.is_alphanumeric() || c == '_' => {}
            _ => return true,
        }
        from = end;
    }
    false
}

/// Lines of `source` mentioning `needle` (textual match, comments included).
/// A line is reported once, however many occurrences it holds.
fn find_offenses(source: &str, needle: &str) -> Vec<Offense> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| mentions(line, needle))
        .map(|(i, line)| Offense {
            line: i + 1,
            text: line.trim().to_string(),
        })
        .collect()
}

/// Scan `sources` for `needles`, panicking with every offending site.
fn assert_clean(layer: &str, sources: &[Source], needles: &[&str]) {
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

// ── The rules ───────────────────────────────────────────────────

#[test]
fn ui_never_reaches_up_into_the_app_layer() {
    let sources = layer_sources("ui");
    assert_scanned(&sources, UI_ANCHORS);
    assert_clean("ui", &sources, &["crate::app"]);
}

#[test]
fn the_neutral_layer_never_reaches_up() {
    let sources = layer_sources("shared");
    assert_scanned(&sources, SHARED_ANCHORS);
    assert_clean("shared", &sources, &["crate::app", "crate::ui"]);
}

// ── The scanner itself (synthetic inputs) ───────────────────────

#[test]
fn scanner_flags_every_reference_form() {
    // A `use` at the top of a file.
    let uses = "use crate::app::constants::TOOL_BASH;\n";
    assert_eq!(
        find_offenses(uses, "crate::app"),
        vec![Offense {
            line: 1,
            text: "use crate::app::constants::TOOL_BASH;".into(),
        }]
    );
    // An inline path inside a function body.
    let inline = "fn cell() -> ChatCell {\n    ChatCell::GoalSeparator { role: crate::app::goal::GoalRole::Executor }\n}\n";
    let hits = find_offenses(inline, "crate::app");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].line, 2);
    // A doc comment — the textual scan covers prose pointers too.
    let doc = "//! State lives in `crate::app::ask_panel::AskPanel`.\n";
    assert_eq!(find_offenses(doc, "crate::app").len(), 1);
    // A line is reported once, however many occurrences it holds.
    let twice = "use crate::app::x; use crate::app::y;\n";
    assert_eq!(find_offenses(twice, "crate::app").len(), 1);
    assert_eq!(find_offenses(twice, "crate::app")[0].line, 1);
    // The bare module path (`use crate::app;`) is a reference like any other.
    assert_eq!(find_offenses("use crate::app;\n", "crate::app").len(), 1);
}

#[test]
fn scanner_ignores_lookalikes() {
    let benign = [
        "use crate::shared::panels::ask::AskPanel;",
        "use crate::app_ui::Thing;", // same prefix, different path segment
        "// the app keeps the live panel", // the word alone is not a reference
        "use crate::protocol::AskQuestion;",
    ]
    .join("\n");
    assert_eq!(find_offenses(&benign, "crate::app"), Vec::new());
    assert_eq!(find_offenses(&benign, "crate::ui"), Vec::new());
    // The same text under a needle it does contain.
    let hits = find_offenses(&benign, "crate::shared");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].line, 1);
}
