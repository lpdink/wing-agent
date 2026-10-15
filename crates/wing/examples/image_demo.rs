//! `image_demo` — put one image file on screen through the real `ui::image` pipeline.
//!
//! Same code path the TUI uses (detect → store → paint), minus the chat view, so a terminal
//! can be checked against the compatibility matrix without a model, a gateway or a session:
//!
//! ```text
//! cargo run -q -p wing --example image_demo -- /tmp/plot.png
//! ```
//!
//! Keys: `q` / `Esc` quit, `r` invalidate (re-encode from scratch, as a resize or
//! `terminal.clear()` would), `-` / `+` shrink / grow the encode target to exercise the
//! encoder at several sizes.
//!
//! `--check` runs the same plan/paint code against an in-memory backend and prints what it
//! produced, so the pipeline can be verified without a TTY:
//!
//! ```text
//! cargo run -q -p wing --example image_demo -- --check /tmp/plot.png
//! ```
//!
//! Headless behaviour: with no TTY, or on a terminal without a graphics protocol, the demo
//! prints why and exits 0 — it never panics and never leaves the terminal in raw mode.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use wing::tui::{WingTerminal, init_terminal, restore_terminal};
use wing::ui::image::{
    CellPixels, DEFAULT_DETECT_TIMEOUT, ImageMeta, ImageProtocol, ImageState, ImageStore,
    ImageSupport, MetaState, ReadyImage, paint,
};

/// The cell pixel size `--check` pretends the terminal reported.
const CHECK_CELL: CellPixels = CellPixels::new(10, 20);

/// The viewport `--check` renders into.
const CHECK_AREA: (u16, u16) = (80, 24);

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).peekable();
    let mut check = false;
    let mut path = None;
    for arg in args.by_ref() {
        match arg.to_str() {
            Some("--check") => check = true,
            _ => {
                path = Some(PathBuf::from(arg));
                break;
            }
        }
    }
    let path = path.or_else(|| args.next().map(PathBuf::from));
    let Some(path) = path else {
        let _ = writeln!(io::stderr(), "usage: image_demo [--check] <image path>");
        return ExitCode::FAILURE;
    };
    let outcome = if check { check_mode(&path) } else { run(&path) };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(io::stderr(), "image_demo: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Run the real plan/paint path against an in-memory backend and report what came out.
fn check_mode(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let support = ImageSupport::from_parts(ImageProtocol::Kitty, CHECK_CELL, false);
    let mut store = ImageStore::new(support);
    let (width, height) = CHECK_AREA;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    let area = Rect::new(0, 0, width, height);
    let [status_area, image_area] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).areas(area);

    // First pass: the header probe has not answered yet.
    let mut frame = plan_frame(&mut store, path, image_area, 1);
    let first = frame.status.clone();
    draw(&mut terminal, &frame, status_area, image_area, path)?;
    // Wait for the worker the way the event loop does — but only for as long as the store
    // says it is still working, so a missing file fails fast instead of looking like a hang.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while frame.pending {
        store.poll();
        frame = plan_frame(&mut store, path, image_area, 1);
        if !frame.pending {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = writeln!(io::stdout(), "check: timed out; first status: {first}");
            return Err("the pipeline never produced an image".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let covered = draw(&mut terminal, &frame, status_area, image_area, path)?;
    let stats = store.stats();
    let backend = terminal.backend();
    let painted = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .filter(|(x, y)| backend.buffer()[(*x, *y)].symbol().contains('\u{10EEEE}'))
        .count();
    let _ = writeln!(
        io::stdout(),
        "check: first={first}\ncheck: {}\ncheck: covered={covered:?} placeholders={painted} \
         cached={} bytes={} memo={}",
        frame.status,
        stats.cached,
        stats.cached_bytes,
        stats.memo
    );
    if painted == 0 {
        return Err(format!("nothing was painted — {}", frame.status).into());
    }
    Ok(())
}

fn run(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let _ = writeln!(
            io::stdout(),
            "image_demo: not a terminal — nothing to draw. Run this in a real terminal to \
             verify the graphics path."
        );
        return Ok(());
    }

    let mut terminal = init_terminal()?;
    // Upstream's rule: query the terminal after entering the alternate screen, before the
    // event loop starts reading stdin.
    let support = ImageSupport::detect(DEFAULT_DETECT_TIMEOUT);
    if !support.is_enabled() {
        restore_terminal(&mut terminal)?;
        let _ = writeln!(
            io::stdout(),
            "image_demo: no terminal graphics protocol detected (kitty/sixel/iTerm2) — the \
             TUI keeps its text rendering on this terminal."
        );
        return Ok(());
    }

    let mut store = ImageStore::new(support.clone());
    let mut width_scale: u16 = 1;
    let outcome = event_loop(&mut terminal, &mut store, path, &mut width_scale);
    restore_terminal(&mut terminal)?;
    outcome
}

/// What to draw this frame, decided *before* the borrow of the frame's buffer.
struct Frame {
    status: String,
    image: Option<ReadyImage>,
    offset: (i16, i16),
    /// The store is still working (probe or encode in flight) — `--check` waits for that.
    pending: bool,
}

fn plan_frame(store: &mut ImageStore, path: &Path, area: Rect, width_scale: u16) -> Frame {
    let meta = match store.meta(path) {
        MetaState::Known(meta) => meta,
        MetaState::Unknown => {
            return Frame {
                status: "Pending — reading the file header".to_string(),
                image: None,
                offset: (0, 0),
                pending: true,
            };
        }
        MetaState::Unavailable(reason) => {
            return Frame {
                status: format!("Unavailable — {reason}"),
                image: None,
                offset: (0, 0),
                pending: false,
            };
        }
    };

    let Some(target) = request_target(meta, store.support(), area, width_scale) else {
        return Frame {
            status: "no room to draw".to_string(),
            image: None,
            offset: (0, 0),
            pending: false,
        };
    };

    match store.request(path, target) {
        ImageState::Ready(image) => {
            let size = image.size();
            let offset = (
                (area.width.saturating_sub(size.width) / 2) as i16,
                (area.height.saturating_sub(size.height) / 2) as i16,
            );
            Frame {
                status: format!(
                    "Ready — {} · {}x{} cells · target {}x{}",
                    image.protocol().name(),
                    size.width,
                    size.height,
                    target.width,
                    target.height
                ),
                image: Some(image),
                offset,
                pending: false,
            }
        }
        ImageState::Pending => Frame {
            status: format!(
                "Pending — encoding for {}x{} cells",
                target.width, target.height
            ),
            image: None,
            offset: (0, 0),
            pending: true,
        },
        ImageState::Unavailable(reason) => Frame {
            status: format!("Unavailable — {reason}"),
            image: None,
            offset: (0, 0),
            pending: false,
        },
    }
}

fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
    frame: &Frame,
    status_area: Rect,
    image_area: Rect,
    path: &Path,
) -> Result<Option<Rect>, Box<dyn std::error::Error>>
where
    <B as Backend>::Error: 'static,
{
    let mut covered = None;
    terminal.draw(|f| {
        let lines = vec![
            Line::styled(
                "image_demo — ui::image pipeline",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::from(frame.status.clone()),
            Line::from(format!("file: {}", path.display())),
            Line::styled(
                "q quit · r invalidate (re-encode) · - / + shrink / grow the target",
                Style::default().fg(Color::DarkGray),
            ),
        ];
        Paragraph::new(lines).render(status_area, f.buffer_mut());

        // Painting last is the contract: nothing may be drawn over the image afterwards.
        if let Some(image) = &frame.image {
            covered = paint(image, image_area, frame.offset, f.buffer_mut());
        }
    })?;
    Ok(covered)
}

/// The encode target for `area`: the image's aspect ratio fitted into it, then scaled by the
/// demo's width knob. Mirrors what the chat view will do with its reserved lines.
fn request_target(
    meta: ImageMeta,
    support: &ImageSupport,
    area: Rect,
    width_scale: u16,
) -> Option<Size> {
    let cell = support.cell_pixel_size()?;
    if area.width == 0 || area.height == 0 || meta.px_w == 0 || meta.px_h == 0 {
        return None;
    }
    let natural_cols = meta.px_w.div_ceil(u32::from(cell.width)).max(1);
    let cols = natural_cols
        .min(u32::from(area.width))
        .checked_div(u32::from(width_scale))
        .unwrap_or(natural_cols)
        .max(1);
    // rows = cols × (cell width / cell height) × (image height / image width)
    let rows = f64::from(cols) * f64::from(cell.width) * f64::from(meta.px_h)
        / f64::from(cell.height)
        / f64::from(meta.px_w);
    let rows = (rows.ceil() as u32).clamp(1, u32::from(area.height));
    Some(Size::new(cols as u16, rows as u16))
}

fn event_loop(
    terminal: &mut WingTerminal,
    store: &mut ImageStore,
    path: &Path,
    width_scale: &mut u16,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        // Absorb whatever the worker finished since the last frame.
        store.poll();

        let area = terminal.size()?;
        let [status_area, image_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).areas(Rect::new(
                0,
                0,
                area.width,
                area.height,
            ));
        let frame = plan_frame(store, path, image_area, *width_scale);
        draw(terminal, &frame, status_area, image_area, path)?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('r') => store.invalidate(),
                KeyCode::Char('-') => *width_scale = (*width_scale + 1).min(8),
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    *width_scale = width_scale.saturating_sub(1).max(1);
                }
                _ => {}
            },
            Event::Resize(..) => store.invalidate(),
            _ => {}
        }
    }
}
