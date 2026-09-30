//! `render_probe` — render arbitrary text through the real TUI markdown
//! pipeline and print the result.
//!
//! A debugging tool for rendering anomalies (wrong code-block detection,
//! wrapping, thinking colors, streaming drift): feed it the exact text that
//! rendered wrong — typically a `reasoning_content` / `content` field out of
//! `$WING_HOME/core/sessions/<id>/history.jsonl` — and see what the renderer
//! makes of it, with the same code paths the TUI uses.
//!
//! Two views are available:
//!
//! - **composed view** (default): `full_lines` / `StreamingRender` output —
//!   exactly what the chat cell shows (prefix, thinking recolor, hard wrap).
//! - **`--kinds` view**: the markdown IR before composition, one line per
//!   segment run annotated with its [`SegmentKind`] (`T` prose, `H` heading,
//!   `i` inline code, `C` code block, `L` link, `M` marker, `B` border,
//!   `G` gutter, `$` math) — this is the view that answers "why is this line
//!   rendered as code?".
//!
//! `--chunk` drives the incremental `StreamingRender` (the production
//! streaming path) instead of the one-shot full render, and `--check`
//! reconciles the two — the invariant the TUI depends on at turn end.
//!
//! Usage:
//! ```text
//! # whole-file diagnostic (reasoning profile)
//! cargo run -p wing --example render_probe -- --profile thinking /tmp/reasoning.md
//!
//! # pull the field straight out of a session log
//! cargo run -p wing --example render_probe -- \
//!     --jsonl ~/.wing/core/sessions/<id>/history.jsonl --index 344 \
//!     --field reasoning --kinds --range 400:440
//!
//! # does the streaming path converge to the full render for this text?
//! cargo run -p wing --example render_probe -- --chunk 32 --check /tmp/reasoning.md
//! ```
//!
//! Not part of `cargo test` (examples are only built/run explicitly).

use std::fmt::Write as _;
use std::fs;
use std::io::{IsTerminal, Read, Write};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use wing::config::ThemePalette;
use wing::config::rendering::MathMode;
use wing::render::markdown::Profile;
use wing::render::markdown::render_markdown_lines_with;
use wing::render::markdown::stream::{StreamingRender, full_lines};
use wing::render::markdown::types::SegmentKind;
use wing::render::markdown::{MarkdownLine, RenderOpts};

const USAGE: &str = "\
render_probe — render text through the TUI markdown pipeline

USAGE:
    render_probe [OPTIONS] [FILE]

    FILE is read as-is; `-` or no FILE reads stdin.

INPUT:
        --jsonl <PATH>      read a session history.jsonl and extract one record
        --index <N>         zero-based record index within the file (required with --jsonl)
        --field <F>         reasoning | content   (default: reasoning; falls back to the other)

RENDER:
    -p, --profile <P>       thinking | content    (default: thinking)
    -w, --width <N>         render width in columns (default: 120)
        --math <M>          text | off            (default: text; off = the
                            pre-math behavior: LaTeX source is left verbatim)
        --chunk <N>         drive the streaming engine in N-byte chunks (production path)
        --no-finalize       keep the streaming engine's live state (skip the turn-end reconcile)
        --check             after --chunk, compare the resting (and finalized) state
                            against the full reference render
    -k, --kinds             print the markdown IR with per-line SegmentKind annotations
                            (mutually exclusive with --chunk: the IR has no incremental form)
        --range <A:B>       1-based inclusive line range of the OUTPUT to print (A may be empty)
        --plain             no ANSI colors
    -h, --help              this text
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).peekable();
    let mut file: Option<String> = None;
    let mut jsonl: Option<String> = None;
    let mut index: Option<usize> = None;
    let mut field = "reasoning".to_string();
    let mut profile = Profile::Thinking;
    let mut width: u16 = 120;
    let mut math = MathMode::Text;
    let mut chunk: Option<usize> = None;
    let mut finalize = true;
    let mut check = false;
    let mut kinds = false;
    let mut range: Option<(usize, usize)> = None;
    let mut plain = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                let mut out = std::io::stdout();
                out.write_all(USAGE.as_bytes())?;
                return Ok(());
            }
            "--jsonl" => jsonl = Some(next_value(&mut args, "--jsonl")?),
            "--index" => {
                index = Some(next_value(&mut args, "--index")?.parse()?);
            }
            "--field" => field = next_value(&mut args, "--field")?,
            "-p" | "--profile" => {
                profile = match next_value(&mut args, "--profile")?.as_str() {
                    "thinking" => Profile::Thinking,
                    "content" => Profile::Content,
                    other => return Err(format!("unknown profile {other:?}").into()),
                };
            }
            "-w" | "--width" => width = next_value(&mut args, "--width")?.parse()?,
            "--math" => {
                math = match next_value(&mut args, "--math")?.as_str() {
                    "text" => MathMode::Text,
                    "off" => MathMode::Off,
                    other => return Err(format!("unknown math mode {other:?}").into()),
                };
            }
            "--chunk" => chunk = Some(next_value(&mut args, "--chunk")?.parse()?),
            "--no-finalize" => finalize = false,
            "--check" => check = true,
            "-k" | "--kinds" => kinds = true,
            "--plain" => plain = true,
            "--range" => {
                let spec = next_value(&mut args, "--range")?;
                let (a, b) = spec
                    .split_once(':')
                    .ok_or_else(|| format!("--range wants A:B, got {spec:?}"))?;
                let a = if a.is_empty() { 1 } else { a.parse()? };
                range = Some((a, b.parse()?));
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option {other:?} (try --help)").into());
            }
            other => file = Some(other.to_string()),
        }
    }

    if kinds && chunk.is_some() {
        return Err(
            "--kinds and --chunk are mutually exclusive (the IR view is a \
                    one-shot full parse; use --check for the streaming path)"
                .into(),
        );
    }

    let text = load_text(file, jsonl, index, &field)?;
    let palette = ThemePalette {
        math_mode: math,
        ..ThemePalette::default()
    };
    let mut out = std::io::stdout();

    writeln!(
        out,
        "\u{2500}\u{2500} render_probe: profile={profile:?} width={width} \
         chars={} lines={} math={math:?} {}",
        text.chars().count(),
        text.lines().count(),
        if kinds { "view=kinds" } else { "view=composed" },
    )?;

    if check {
        if chunk.is_none() {
            return Err("--check needs --chunk (it reconciles the streaming path)".into());
        }
        let (report, diverged) =
            check_streaming(&text, chunk.unwrap_or(1), width, profile, &palette);
        for line in report {
            writeln!(out, "{line}")?;
        }
        // Usable as a gate: the exit code carries the verdict.
        if diverged {
            std::process::exit(1);
        }
        return Ok(());
    }

    let lines: Vec<Line<'static>> = if let Some(chunk) = chunk {
        let mut stream = StreamingRender::new(profile);
        for piece in chunks_of(&text, chunk) {
            stream.push(piece);
            // Sync every chunk — exactly what the UI does per frame.
            let _ = stream.lines(width, &palette);
        }
        if finalize {
            stream.finalize(width, &palette);
        }
        stream.lines(width, &palette).to_vec()
    } else if kinds {
        // IR view: the markdown layer without the cell compose (prefix / hard
        // wrap), so kinds stay attached to the lines that produced them.
        let opts = RenderOpts::new(profile, true).with_math(math);
        let md = render_markdown_lines_with(&text, Some(width.saturating_sub(2)), &palette, opts);
        print_ir(&mut out, &md, range, plain)?;
        return Ok(());
    } else {
        full_lines(&text, width, profile, &palette)
    };

    print_lines(&mut out, &lines, range, plain)?;
    Ok(())
}

/// Print the composed (TUI-equivalent) lines.
fn print_lines(
    out: &mut impl Write,
    lines: &[Line<'static>],
    range: Option<(usize, usize)>,
    plain: bool,
) -> std::io::Result<()> {
    let (from, to) = range.unwrap_or((1, lines.len()));
    let gutter = to.to_string().len();
    for (i, line) in lines.iter().enumerate() {
        let n = i + 1;
        if n < from || n > to {
            continue;
        }
        let mut text = String::new();
        for span in &line.spans {
            if plain {
                text.push_str(&span.content);
            } else {
                let _ = write!(text, "{}{}{}", sgr(span.style), span.content, RESET);
            }
        }
        writeln!(out, "{n:>gutter$} | {text}")?;
    }
    Ok(())
}

/// Print the pre-compose markdown IR with per-segment kind annotations.
fn print_ir(
    out: &mut impl Write,
    lines: &[MarkdownLine],
    range: Option<(usize, usize)>,
    plain: bool,
) -> std::io::Result<()> {
    let (from, to) = range.unwrap_or((1, lines.len()));
    for (i, line) in lines.iter().enumerate() {
        if i + 1 < from || i + 1 > to {
            continue;
        }
        let kinds: String = line.segments.iter().map(|s| kind_letter(s.kind)).collect();
        let mut text = String::new();
        for seg in &line.segments {
            if plain {
                text.push_str(&seg.text);
            } else {
                let _ = write!(text, "{}{}{}", sgr(seg.style), seg.text, RESET);
            }
        }
        writeln!(out, "{:>4} [{kinds}] {text}", i + 1)?;
    }
    Ok(())
}

/// Stream the text and reconcile the incremental resting state against the
/// reference full render — the TUI's turn-end invariant. Returns the report
/// lines and whether anything diverged (the caller turns that into the exit
/// code).
fn check_streaming(
    text: &str,
    chunk: usize,
    width: u16,
    profile: Profile,
    palette: &ThemePalette,
) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut stream = StreamingRender::new(profile);
    for piece in chunks_of(text, chunk) {
        stream.push(piece);
        let _ = stream.lines(width, palette);
    }
    let resting = stream.lines(width, palette).to_vec();
    let reference = full_lines(text, width, profile, palette);
    out.push(format!(
        "resting(before finalize) vs full render: {} lines vs {} lines",
        resting.len(),
        reference.len()
    ));
    let resting_diff = diff_summary(&resting, &reference, "resting");
    let resting_diverged = diverged(&resting, &reference);
    out.extend(resting_diff);

    stream.finalize(width, palette);
    let finalized = stream.lines(width, palette).to_vec();
    out.push(format!(
        "after finalize: {} lines vs {} lines",
        finalized.len(),
        reference.len()
    ));
    out.extend(diff_summary(&finalized, &reference, "finalized"));
    (out, resting_diverged || diverged(&finalized, &reference))
}

/// Whether two renders differ in any line's text or styles.
fn diverged(a: &[Line<'static>], b: &[Line<'static>]) -> bool {
    a.len() != b.len()
        || a.iter().zip(b).any(|(x, y)| {
            x.spans.len() != y.spans.len()
                || x.spans
                    .iter()
                    .zip(&y.spans)
                    .any(|(p, q)| p.content != q.content || p.style != q.style)
        })
}

/// First mismatches (line text + styles) between two renders.
fn diff_summary(a: &[Line<'static>], b: &[Line<'static>], label: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut shown = 0usize;
    for i in 0..a.len().max(b.len()) {
        let (la, lb) = (a.get(i), b.get(i));
        let same = match (la, lb) {
            (Some(la), Some(lb)) => {
                la.spans.len() == lb.spans.len()
                    && la
                        .spans
                        .iter()
                        .zip(&lb.spans)
                        .all(|(x, y)| x.content == y.content && x.style == y.style)
            }
            _ => false,
        };
        if same {
            continue;
        }
        if shown < 8 {
            let render = |l: Option<&Line<'static>>| match l {
                Some(l) => format!(
                    "text={:?} fg={:?}",
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>(),
                    l.spans.first().map(|s| s.style.fg)
                ),
                None => "<missing>".to_string(),
            };
            out.push(format!(
                "  [{label}] line {i}: got {} / want {}",
                render(la),
                render(lb)
            ));
        }
        shown += 1;
    }
    out.push(if shown == 0 {
        format!("  [{label}] identical")
    } else {
        format!("  [{label}] {shown} differing line(s)")
    });
    out
}

fn kind_letter(kind: SegmentKind) -> char {
    match kind {
        SegmentKind::Text => 'T',
        SegmentKind::Heading => 'H',
        SegmentKind::InlineCode => 'i',
        SegmentKind::CodeBlock => 'C',
        SegmentKind::Link => 'L',
        SegmentKind::Marker => 'M',
        SegmentKind::Border => 'B',
        SegmentKind::Gutter => 'G',
        SegmentKind::Math => '$',
    }
}

/// Split `text` into `size`-byte chunks on char boundaries.
fn chunks_of(text: &str, size: usize) -> Vec<&str> {
    let size = size.max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = (start + size).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

fn load_text(
    file: Option<String>,
    jsonl: Option<String>,
    index: Option<usize>,
    field: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(path) = jsonl {
        let index = index.ok_or("--jsonl needs --index")?;
        let raw = fs::read_to_string(&path)?;
        let record = raw
            .lines()
            .nth(index)
            .ok_or_else(|| format!("{path}: no record #{index}"))?;
        let value: serde_json::Value = serde_json::from_str(record)?;
        let primary = match field {
            "content" => "content",
            _ => "reasoning_content",
        };
        let fallback = if primary == "content" {
            "reasoning_content"
        } else {
            "content"
        };
        for key in [primary, fallback] {
            if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    return Ok(text.to_string());
                }
            }
        }
        return Err(format!("{path}#{index}: no non-empty {primary}/{fallback} field").into());
    }
    match file.as_deref() {
        None | Some("-") => {
            let mut buf = String::new();
            if std::io::stdin().is_terminal() {
                return Err(
                    "no FILE given and stdin is a terminal — pass a file or pipe text".into(),
                );
            }
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
        Some(path) => Ok(fs::read_to_string(path)?),
    }
}

fn next_value(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

const RESET: &str = "\u{1b}[0m";

/// ratatui `Style` → SGR escape sequence.
fn sgr(style: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    if let Some(fg) = style.fg {
        codes.push(color_code(fg, false));
    }
    if let Some(bg) = style.bg {
        codes.push(color_code(bg, true));
    }
    let m = style.add_modifier;
    for (flag, code) in [
        (Modifier::BOLD, 1),
        (Modifier::DIM, 2),
        (Modifier::ITALIC, 3),
        (Modifier::UNDERLINED, 4),
        (Modifier::REVERSED, 7),
        (Modifier::CROSSED_OUT, 9),
    ] {
        if m.contains(flag) {
            codes.push(code.to_string());
        }
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\u{1b}[{}m", codes.join(";"))
    }
}

fn color_code(color: Color, background: bool) -> String {
    let base = if background { 40 } else { 30 };
    let bright = if background { 100 } else { 90 };
    match color {
        Color::Reset => "0".to_string(),
        Color::Black => base.to_string(),
        Color::Red => (base + 1).to_string(),
        Color::Green => (base + 2).to_string(),
        Color::Yellow => (base + 3).to_string(),
        Color::Blue => (base + 4).to_string(),
        Color::Magenta => (base + 5).to_string(),
        Color::Cyan => (base + 6).to_string(),
        Color::Gray => (base + 7).to_string(),
        Color::DarkGray => bright.to_string(),
        Color::LightRed => (bright + 1).to_string(),
        Color::LightGreen => (bright + 2).to_string(),
        Color::LightYellow => (bright + 3).to_string(),
        Color::LightBlue => (bright + 4).to_string(),
        Color::LightMagenta => (bright + 5).to_string(),
        Color::LightCyan => (bright + 6).to_string(),
        Color::White => (bright + 7).to_string(),
        Color::Indexed(i) => format!("{};5;{i}", if background { 48 } else { 38 }),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", if background { 48 } else { 38 }),
    }
}
