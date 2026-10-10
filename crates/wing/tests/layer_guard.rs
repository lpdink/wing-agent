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
//! `tui` and `cmd` sit *above* `ui` (the terminal lifecycle owns the frames it
//! sends and normalizes them — `tui::draw_frame` → `ui::emoji_width`; the wizard
//! draws its own screen), so those edges are legal and the layer rules do not
//! scan them. The second rule in this file is about that ownership instead: only
//! `tui/mod.rs` may hand a frame to a terminal.
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

use std::collections::BTreeSet;
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
        if is_whole_path(source, first, last + 1, needle)
            && !offenses.iter().any(|o| o.line == line)
        {
            offenses.push(Offense {
                line,
                text: original.get(line - 1).unwrap_or(&"").trim().to_string(),
            });
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

/// Anchor files the guard must actually see (`assert_scanned` panics when one is
/// missing): every layer directory registers a few of its files here, so a walk
/// that stopped early cannot let the rules pass vacuously.
const UI_ANCHORS: &[&str] = &["ui/panel.rs", "ui/chat_view/cell.rs", "ui/cells/ask_msg.rs"];

/// The neutral layer's anchors — same contract; every panel package registers
/// its files here (the settings package included).
const SHARED_ANCHORS: &[&str] = &[
    "shared/mod.rs",
    "shared/constants.rs",
    "shared/tips.rs",
    "shared/doc_edit.rs",
    "shared/panels/mod.rs",
    "shared/panels/ask.rs",
    "shared/panels/picker.rs",
    // The settings panel package (07).
    "shared/panels/settings/mod.rs",
    "shared/panels/settings/doc.rs",
    "shared/panels/settings/tree.rs",
    "shared/panels/settings/edit.rs",
    "shared/panels/settings/list.rs",
    "shared/panels/settings/search.rs",
    "shared/panels/settings/problems.rs",
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

// ── The frame entry point ───────────────────────────────────────
//
// `ui::emoji_width` normalizes a finished frame before ratatui diffs it, and
// `tui::draw_frame` is the one function that does both. A *second* draw path
// calling `Terminal::draw` itself loses that silently: the emoji drift (#181)
// comes back, and nothing shows it — `TestBackend` copies cells instead of
// modelling a cursor, so no test sees it, and on screen the damage is
// intermittent and lives only on the terminal (the buffer, and therefore the
// copy, stays right). The rule below is about *who may draw a frame*: only
// `tui/mod.rs`.
//
// It is a text scan, not a type check, and it leans on two of the tree's
// conventions: a test module is the file tail (`#[cfg(test)]` + `mod`), and a
// test-only file is declared as such (`#[cfg(test)] mod tests;`, which owns a
// whole directory). It can under-report — a call in a file that breaks either
// convention, or a call whose receiver and argument carry neither of the
// signals `draws_a_frame` reads — and then the fix is to make the file (or the
// call) say what it is, not to widen this rule until it stops meaning
// anything.

/// The one file allowed to hand a frame to a terminal.
const FRAME_ENTRY_POINT: &str = "tui/mod.rs";

/// The calls that hand a frame to a terminal: `Terminal::draw` and its
/// `try_draw` sibling both end in `flush`, i.e. in the buffer diff.
const DRAW_CALLS: [&str; 2] = [".draw(", ".try_draw("];

/// The names a terminal goes by in this tree — one of the two signals that a
/// `.draw(` call is a frame going to the screen.
const TERMINAL_NAMES: [&str; 2] = ["terminal", "term"];

/// Production text the guard must really be scanning: `(path, anchor)`.
///
/// [`assert_scanned`] proves the walk reached the files; these prove the
/// production/test split did not swallow their production code, which would
/// turn the guard into a no-op that still passes.
const FRAME_ANCHORS: &[(&str, &str)] = &[
    ("tui/mod.rs", "pub fn draw_frame<B, F>"),
    ("app/mod.rs", "crate::tui::draw_frame(terminal"),
    ("cmd/setup.rs", "fn draw_setup_frame<B: Backend>("),
    // A file whose `#[cfg(test)]` markers sit on single items before its test
    // module: the production code around them must stay in the scan.
    (
        "ui/chat_view/model.rs",
        "pub fn push_pending(&mut self, request_id: String",
    ),
];

/// The production part of `text`: everything before the file's own test module.
///
/// That module is the tail of the file and is written as a top-level
/// `#[cfg(test)]` with `mod …` under it. The marker this must *not* cut on is
/// the one `#[cfg(test)]` puts on a single item (a test-only accessor, a
/// counter field): production code follows those, so cutting there would hide
/// it instead of skipping test code.
fn production_text(text: &str) -> &str {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut offset = 0;
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("#[cfg(test)]")
            && lines[index + 1..]
                .iter()
                .find(|next| !is_insignificant(next))
                .is_some_and(|next| opens_a_module(next))
        {
            return &text[..offset];
        }
        offset += line.len();
    }
    text
}

/// A line a `mod` declaration may sit behind: blank, a comment, an attribute.
fn is_insignificant(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || line.starts_with("//") || line.starts_with("#[")
}

/// Whether the line declares a module (`mod tests {`, `pub(crate) mod x;`).
fn opens_a_module(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("mod ") || line.starts_with("pub mod ") || line.starts_with("pub(crate) mod ")
}

/// Every module `text` declares at its top level, as `(test_only, name)`.
///
/// For a declaration a `#[cfg(test)]` marker in front of it *is* the module's
/// configuration, so it is reported as test-only; an inline module (`mod x {`)
/// has no file of its own and resolves to nothing, which the caller filters.
fn declared_modules(text: &str) -> Vec<(bool, String)> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut modules = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let (marked, line) = if line.starts_with("#[cfg(test)]") {
            match lines[index + 1..]
                .iter()
                .find(|next| !is_insignificant(next))
            {
                Some(next) => (true, *next),
                None => break,
            }
        } else {
            (false, *line)
        };
        if let Some(name) = module_name(line) {
            modules.push((marked, name.to_owned()));
        }
    }
    modules
}

/// The name a module line declares (`mod tests;`, `pub mod planner {`).
fn module_name(line: &str) -> Option<&str> {
    let line = line.trim();
    let line = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line)
        .strip_prefix("mod ")?
        .trim_start();
    let end = line
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(line.len());
    let (name, tail) = line.split_at(end);
    (!name.is_empty() && tail.trim_start().starts_with([';', '{'])).then_some(name)
}

/// The two paths a `mod NAME;` can name, in Rust's own resolution order.
fn module_candidates(declaring: &str, name: &str) -> [String; 2] {
    let dir = declaring.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    [
        format!("{prefix}{name}.rs"),
        format!("{prefix}{name}/mod.rs"),
    ]
}

/// The files that are test code: the ones a `#[cfg(test)] mod` names, plus
/// everything those declare in turn — `mod tests;` owns a whole directory, and
/// the files in it carry no marker of their own.
fn test_only_files(sources: &[Source]) -> BTreeSet<String> {
    let present: BTreeSet<&str> = sources.iter().map(|source| source.path.as_str()).collect();
    let mut test_only = BTreeSet::new();
    let mut pending: Vec<String> = sources
        .iter()
        .flat_map(|source| {
            declared_modules(&source.text)
                .into_iter()
                .filter(|(test_only, _)| *test_only)
                .flat_map(|(_, name)| module_candidates(&source.path, &name))
        })
        .filter(|path| present.contains(path.as_str()))
        .collect();
    while let Some(path) = pending.pop() {
        if !test_only.insert(path.clone()) {
            continue;
        }
        let Some(source) = sources.iter().find(|source| source.path == path) else {
            continue;
        };
        for (_, name) in declared_modules(&source.text) {
            pending.extend(
                module_candidates(&source.path, &name)
                    .into_iter()
                    .filter(|child| present.contains(child.as_str())),
            );
        }
    }
    test_only
}

/// Whether `path` is test code: a test module itself, or a file owned by one.
fn is_test_file(path: &str, test_only: &BTreeSet<String>) -> bool {
    let mut current = path.to_owned();
    loop {
        if test_only.contains(&current) {
            return true;
        }
        match owner_module(&current) {
            Some(owner) => current = owner,
            None => return false,
        }
    }
}

/// The `mod.rs` owning `path`: `a/b/c.rs` and `a/b/c/mod.rs` are both owned by
/// `a/b/mod.rs` (the tree keeps one at every level, so the walk terminates).
fn owner_module(path: &str) -> Option<String> {
    let dir = match path.strip_suffix("/mod.rs") {
        // `a/b/mod.rs` is owned by `a/mod.rs`.
        Some(module_dir) => module_dir.rsplit_once('/').map(|(up, _)| up).unwrap_or(""),
        None => path.rsplit_once('/').map(|(dir, _)| dir)?,
    };
    Some(if dir.is_empty() {
        "mod.rs".to_owned()
    } else {
        format!("{dir}/mod.rs")
    })
}

/// Whether the draw call at `at` hands a frame to a terminal.
///
/// Two signals, either one enough: the receiver says `terminal` / `term` (the
/// tree's naming), or the call is handed a render callback — a closure literal,
/// which is what both ratatui entry points take and nothing else in this tree
/// does. A call that says neither is somebody else's `.draw(`: `app.draw(terminal)`
/// (the app's own frame builder) or `self.inner.borrow_mut().draw(content)` (a
/// `Backend::draw` delegation).
///
/// The limit is deliberate: a terminal bound to a name carrying neither signal
/// (`t.draw(render)`) would be missed. The scan is coarse by contract — it leans
/// on how the tree writes things down, and review polices the rest.
fn draws_a_frame(production: &str, at: usize, call: &str) -> bool {
    let line_start = production[..at]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    TERMINAL_NAMES
        .iter()
        .any(|name| production[line_start..at].contains(name))
        || production[at + call.len()..].trim_start().starts_with('|')
}

/// `(line, text)` of every frame draw in the production part of `text`.
fn draw_offenses(text: &str) -> Vec<(usize, String)> {
    let production = production_text(text);
    let mut offenses: Vec<(usize, String)> = DRAW_CALLS
        .iter()
        .flat_map(|call| {
            production.match_indices(call).filter_map(|(at, _)| {
                if !draws_a_frame(production, at, call) {
                    return None;
                }
                let line = production[..at].matches('\n').count() + 1;
                let start = production[..at]
                    .rfind('\n')
                    .map_or(0, |newline| newline + 1);
                let end = production[at..]
                    .find('\n')
                    .map_or(production.len(), |newline| at + newline);
                Some((line, production[start..end].trim().to_owned()))
            })
        })
        .collect();
    offenses.sort();
    offenses.dedup();
    offenses
}

#[test]
fn only_the_frame_entry_point_draws_a_frame() {
    let mut sources = Vec::new();
    collect(&PathBuf::from(SRC), &mut sources);
    assert_scanned(&sources, &[FRAME_ENTRY_POINT, "app/mod.rs", "cmd/setup.rs"]);

    // The scan must really see the production code it is about.
    for (path, anchor) in FRAME_ANCHORS {
        let source = sources
            .iter()
            .find(|source| source.path == *path)
            .unwrap_or_else(|| panic!("guard is not scanning {path}"));
        assert!(
            production_text(&source.text).contains(anchor),
            "guard is not scanning the production part of {path} (`{anchor}`)"
        );
    }

    let test_only = test_only_files(&sources);
    let mut report = String::new();
    for source in &sources {
        if source.path == FRAME_ENTRY_POINT || is_test_file(&source.path, &test_only) {
            continue;
        }
        for (line, text) in draw_offenses(&source.text) {
            report.push_str(&format!("\n  {}:{line}  {text}", source.path));
        }
    }
    assert!(
        report.is_empty(),
        "a frame is drawn outside `tui::draw_frame`, so it never gets the wire \
         pass (`ui::emoji_width`) and can shift a row on the terminal:{report}\n\
         Draw it through `crate::tui::draw_frame(terminal, |frame| …)` instead."
    );
}

// ── The frame-entry scan (synthetic inputs) ─────────────────────

fn source(path: &str, text: &str) -> Source {
    Source {
        path: path.to_owned(),
        text: text.to_owned(),
    }
}

#[test]
fn the_draw_scan_sees_production_code_and_skips_test_modules() {
    let production = "fn f(terminal: &mut Terminal<B>) {\n    terminal.draw(|frame| {});\n}\n";
    assert_eq!(
        draw_offenses(production),
        vec![(2, "terminal.draw(|frame| {});".to_owned())]
    );

    // The tail test module, with the same call inside it, is out of scope.
    let with_tests = "fn f() {}\n\n#[cfg(test)]\nmod tests {\n    fn t(term: &mut Terminal<TestBackend>) {\n        term.draw(|frame| {});\n    }\n}\n";
    assert_eq!(draw_offenses(with_tests), Vec::new());

    // `#[cfg(test)]` on a single item does not open a module: what follows it is
    // production code, and a draw call there is an offense.
    let single_item = "#[cfg(test)]\nfn helper() {}\n\nfn f(terminal: &mut Terminal<B>) {\n    terminal.draw(|frame| {});\n}\n";
    assert_eq!(draw_offenses(single_item).len(), 1);

    // `try_draw` ends in the same flush, and is caught too — through the
    // callback signal, the receiver here is named neither `terminal` nor `term`.
    let try_draw = "fn f(t: &mut Terminal<B>) {\n    t.try_draw(|frame| Ok(()));\n}\n";
    assert_eq!(draw_offenses(try_draw).len(), 1);

    // A terminal draw whose argument is a named function is caught by the
    // receiver signal instead.
    let named_argument = "fn f(term: &mut Terminal<B>) {\n    term.draw(render)?;\n}\n";
    assert_eq!(draw_offenses(named_argument).len(), 1);

    // The two shapes that are *not* frame draws: the app's own frame builder
    // (whose argument is the terminal) and a `Backend::draw` delegation.
    let app_draw =
        "fn f() {\n    if let Err(e) = app.draw(terminal) {\n        log(e);\n    }\n}\n";
    assert_eq!(draw_offenses(app_draw), Vec::new());
    let backend_draw = "impl Backend for Wrapper {\n    fn draw<'a, I>(&mut self, content: I) {\n        self.inner.borrow_mut().draw(content);\n    }\n}\n";
    assert_eq!(draw_offenses(backend_draw), Vec::new());

    // The production part of a file whose test module has an attribute in front.
    let attributed = "fn f() {}\n\n#[cfg(test)]\n#[allow(clippy::too_many_lines)]\nmod tests {\n    fn t(term: &mut Terminal<TestBackend>) {\n        term.draw(|frame| {});\n    }\n}\n";
    assert_eq!(draw_offenses(attributed), Vec::new());
}

#[test]
fn a_file_owned_by_a_test_module_is_test_code() {
    let sources = vec![
        source(
            "app/mod.rs",
            "pub fn run() {}\n\n#[cfg(test)]\nmod tests;\n",
        ),
        source("app/tests/mod.rs", "mod core;\nmod wire;\n"),
        source(
            "app/tests/core.rs",
            "fn t(terminal: &mut Terminal<TestBackend>) {}\n",
        ),
        source(
            "app/tests/wire.rs",
            "fn t(term: &mut Terminal<TestBackend>) {}\n",
        ),
        source(
            "ui/settings/mod.rs",
            "pub fn build() {}\n\n#[cfg(test)]\nmod test_support;\n\n#[cfg(test)]\nmod tests;\n",
        ),
        source("ui/settings/test_support.rs", "pub(super) fn view() {}\n"),
        source(
            "ui/settings/tests.rs",
            "fn t(term: &mut Terminal<TestBackend>) {}\n",
        ),
        source("app/runner.rs", "pub fn run() {}\n"),
    ];
    let test_only = test_only_files(&sources);
    for path in [
        "app/tests/mod.rs",
        "app/tests/core.rs",
        "app/tests/wire.rs",
        "ui/settings/test_support.rs",
        "ui/settings/tests.rs",
    ] {
        assert!(is_test_file(path, &test_only), "{path} is test code");
    }
    for path in ["app/mod.rs", "app/runner.rs", "ui/settings/mod.rs"] {
        assert!(!is_test_file(path, &test_only), "{path} is production code");
    }
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
        "fn cell() -> Role {\n    Role::from(crate::app::App::default())\n}\n",
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
        "//! the snapshot mirrors `crate :: app::App`\n",
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
    let inline = "fn f() {\n    let r = crate::app::intent::AppIntent::Quit;\n}\n";
    assert_eq!(
        find_offenses(inline, "crate::app"),
        vec![Offense {
            line: 2,
            text: "let r = crate::app::intent::AppIntent::Quit;".into(),
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
