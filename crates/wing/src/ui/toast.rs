//! Toast — transient notification overlay.
//!
//! Lightweight, self-contained toast component. No external deps, no channels.
//! `App` holds `Option<Toast>`; `draw()` checks expiry (lazy cleanup).

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::border;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;

/// Semantic type of a toast message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Warning,
    Error,
}

impl ToastKind {
    /// Border color for this toast kind.
    fn border_color(self, palette: &ThemePalette) -> Color {
        match self {
            Self::Info => palette.accent,
            Self::Warning => palette.warning,
            Self::Error => palette.danger,
        }
    }

    /// Background tint for this toast kind, paired with the palette's
    /// appearance: a dark tint on a light terminal would be the one opaque
    /// block on the screen (and its default foreground would vanish into it).
    fn bg_color(self, palette: &ThemePalette) -> Color {
        // `terminal` hands the background to the terminal; painting a fixed
        // dark slab would put the terminal's own (dark, on a light scheme)
        // foreground on it. The preset says so explicitly — not inferred from
        // a colour slot (a user override must not flip this).
        if !palette.owns_background {
            return Color::Reset;
        }
        match (palette.appearance, self) {
            (crate::config::Appearance::Dark, Self::Info) => Color::Rgb(20, 40, 45),
            (crate::config::Appearance::Dark, Self::Warning) => Color::Rgb(50, 45, 20),
            (crate::config::Appearance::Dark, Self::Error) => Color::Rgb(50, 20, 20),
            (crate::config::Appearance::Light, Self::Info) => Color::Rgb(226, 242, 245),
            (crate::config::Appearance::Light, Self::Warning) => Color::Rgb(250, 243, 215),
            (crate::config::Appearance::Light, Self::Error) => Color::Rgb(252, 228, 226),
        }
    }
}

/// A transient or persistent notification.
///
/// Timed toasts auto-expire after `expires_at`. Persistent toasts remain
/// visible until explicitly cleared via `App::clear_toast()`.
#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    pub persistent: bool,
    expires_at: Instant,
}

impl Toast {
    /// Create an info toast.
    pub fn info(message: impl Into<String>, duration: Duration) -> Self {
        Self::new(message.into(), ToastKind::Info, duration)
    }

    /// Create a warning toast.
    pub fn warning(message: impl Into<String>, duration: Duration) -> Self {
        Self::new(message.into(), ToastKind::Warning, duration)
    }

    /// Create an error toast.
    pub fn error(message: impl Into<String>, duration: Duration) -> Self {
        Self::new(message.into(), ToastKind::Error, duration)
    }

    /// Create a persistent toast that never auto-expires.
    /// Must be explicitly cleared via `App::clear_toast()`.
    pub fn persistent(message: impl Into<String>, kind: ToastKind) -> Self {
        Self {
            message: message.into(),
            kind,
            persistent: true,
            expires_at: Instant::now(), // ignored when persistent
        }
    }

    fn new(message: String, kind: ToastKind, duration: Duration) -> Self {
        Self {
            message,
            kind,
            persistent: false,
            expires_at: Instant::now() + duration,
        }
    }

    /// Whether this toast has expired.
    /// Persistent toasts never expire — they must be cleared explicitly.
    pub fn is_expired(&self) -> bool {
        !self.persistent && Instant::now() >= self.expires_at
    }

    /// Remaining duration until expiry.
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }
}

/// Maximum toast width (columns).
const MAX_WIDTH: u16 = 50;
/// Minimum terminal width to show toast.
const MIN_TERMINAL_WIDTH: u16 = 20;
/// Horizontal/vertical padding from terminal edge.
const EDGE_PADDING: u16 = 2;

/// Render a toast overlay on the frame.
///
/// Returns the rect that was painted, so callers can drop anything the toast
/// covered (the chat view keeps per-frame link hit boxes — a click on a toast
/// must not open the link underneath it).
pub fn render_toast(
    toast: &Toast,
    area: Rect,
    buf: &mut Buffer,
    palette: &ThemePalette,
) -> Option<Rect> {
    if toast.is_expired() || area.width < MIN_TERMINAL_WIDTH {
        return None;
    }

    // Use display width (handles CJK correctly).
    let content_width = toast.message.width() as u16;
    let toast_width = (content_width + 4)
        .min(MAX_WIDTH)
        .min(area.width - EDGE_PADDING);

    // Estimate height from line wrapping. This is an approximation —
    // Paragraph's internal wrapping may differ slightly for edge cases,
    // but is accurate enough for short toast messages.
    let inner_width = toast_width.saturating_sub(2) as usize; // minus L+R border
    let line_count = toast
        .message
        .lines()
        .map(|l| {
            let w = UnicodeWidthStr::width(l);
            if w == 0 { 1 } else { w.div_ceil(inner_width) }
        })
        .sum::<usize>();
    let toast_height = (line_count as u16 + 2).min(area.height.saturating_sub(2)); // +2 for border

    // Position: top-right, below status bar.
    let x = area.x + area.width.saturating_sub(toast_width + 1);
    let y = area.y + 1;
    let toast_area = Rect::new(x, y, toast_width, toast_height);

    Clear.render(toast_area, buf);

    let kind = toast.kind;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(kind.border_color(palette)))
        .style(Style::default().bg(kind.bg_color(palette)));

    Paragraph::new(toast.message.as_str())
        .style(Style::default().fg(palette.text))
        .block(block)
        .render(toast_area, buf);
    Some(toast_area)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemePalette;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    /// WCAG contrast ratio between two `Color::Rgb`s.
    fn contrast(a: Color, b: Color) -> f32 {
        let luma = |c: Color| -> f32 {
            let Color::Rgb(r, g, b) = c else {
                panic!("expected hex, got {c:?}");
            };
            let ch = |v: u8| {
                let v = f32::from(v) / 255.0;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
        };
        let (la, lb) = (luma(a), luma(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn test_toast_info_constructor() {
        let toast = Toast::info("hello", Duration::from_secs(3));
        assert_eq!(toast.message, "hello");
        assert_eq!(toast.kind, ToastKind::Info);
        assert!(!toast.is_expired());
    }

    #[test]
    fn test_toast_warning_constructor() {
        let toast = Toast::warning("careful", Duration::from_secs(2));
        assert_eq!(toast.message, "careful");
        assert_eq!(toast.kind, ToastKind::Warning);
    }

    #[test]
    fn test_toast_error_constructor() {
        let toast = Toast::error("broke", Duration::from_secs(5));
        assert_eq!(toast.message, "broke");
        assert_eq!(toast.kind, ToastKind::Error);
    }

    #[test]
    fn test_toast_expiry() {
        let toast = Toast::new("gone".into(), ToastKind::Info, Duration::from_millis(0));
        // Zero-duration toast is immediately expired.
        assert!(toast.is_expired());
    }

    #[test]
    fn test_toast_remaining_decreases() {
        let toast = Toast::info("test", Duration::from_secs(10));
        let r1 = toast.remaining();
        std::thread::sleep(Duration::from_millis(10));
        let r2 = toast.remaining();
        assert!(r2 < r1);
    }

    #[test]
    fn test_kind_colors() {
        let palette = p();
        assert_eq!(ToastKind::Info.border_color(&palette), palette.accent);
        assert_eq!(ToastKind::Warning.border_color(&palette), palette.warning);
        assert_eq!(ToastKind::Error.border_color(&palette), palette.danger);
    }

    #[test]
    fn test_kind_bg_colors() {
        assert_eq!(ToastKind::Info.bg_color(&p()), Color::Rgb(20, 40, 45));
        assert_eq!(ToastKind::Warning.bg_color(&p()), Color::Rgb(50, 45, 20));
        assert_eq!(ToastKind::Error.bg_color(&p()), Color::Rgb(50, 20, 20));

        // `terminal` leaves the background to the terminal (Reset), so the
        // body foreground it inherits is always the readable one.
        let terminal = crate::config::ThemePalette::from_config_with_appearance(
            &crate::config::ColorsConfig {
                preset: crate::config::ColorPreset::Terminal,
                ..Default::default()
            },
            None,
        );
        assert!(!terminal.owns_background);
        for kind in [ToastKind::Info, ToastKind::Warning, ToastKind::Error] {
            assert_eq!(kind.bg_color(&terminal), Color::Reset);
        }

        // The light palette swaps in light tints (and `text` flips to a dark
        // foreground, so the copy stays readable on them).
        let light = crate::config::ThemePalette::from_config_with_appearance(
            &crate::config::ColorsConfig {
                preset: crate::config::ColorPreset::Light,
                ..Default::default()
            },
            None,
        );
        for kind in [ToastKind::Info, ToastKind::Warning, ToastKind::Error] {
            assert_ne!(kind.bg_color(&p()), kind.bg_color(&light));
            // The body copy sits on the tint: it must clear WCAG 4.5.
            let bg = kind.bg_color(&light);
            assert!(
                contrast(light.text, bg) >= 4.5,
                "{kind:?} toast copy must be readable on its light tint"
            );
        }
    }

    #[test]
    fn test_render_skips_expired() {
        let toast = Toast::new("x".into(), ToastKind::Info, Duration::from_millis(0));
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
        let area = Rect::new(0, 0, 80, 24);
        let palette = p();
        // Should not panic, just no-op.
        render_toast(&toast, area, &mut buf, &palette);
    }

    #[test]
    fn test_render_skips_narrow_terminal() {
        let toast = Toast::info("hello", Duration::from_secs(3));
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 24));
        let area = Rect::new(0, 0, 10, 24);
        let palette = p();
        render_toast(&toast, area, &mut buf, &palette);
    }

    #[test]
    fn test_render_succeeds_normal() {
        let toast = Toast::info("hello", Duration::from_secs(3));
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
        let area = Rect::new(0, 0, 80, 24);
        let palette = p();
        render_toast(&toast, area, &mut buf, &palette);
        // Verify something was written to the toast area (top-right).
        let cell = &buf[Rect::new(79, 1, 1, 1)];
        // After Clear + render, the cell should have content or bg.
        assert!(
            cell.symbol() != " " || cell.style().bg.is_some(),
            "toast area should have content"
        );
    }

    #[test]
    fn test_persistent_constructor() {
        let toast = Toast::persistent("Reconnecting...", ToastKind::Warning);
        assert_eq!(toast.message, "Reconnecting...");
        assert_eq!(toast.kind, ToastKind::Warning);
        assert!(toast.persistent);
    }

    #[test]
    fn test_persistent_never_expires() {
        let toast = Toast::persistent("forever", ToastKind::Info);
        // Persistent toast never expires regardless of elapsed time.
        assert!(!toast.is_expired());
        std::thread::sleep(Duration::from_millis(10));
        assert!(!toast.is_expired());
    }

    #[test]
    fn test_non_persistent_default() {
        let toast = Toast::info("timed", Duration::from_secs(5));
        assert!(!toast.persistent);
    }
}
