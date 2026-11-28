//! Presentation shared by the session picker and sidebar.

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::protocol::SessionStatus;
use crate::shared::session_tree::Crew;

pub(super) const UNTITLED: &str = "(untitled)";
pub(super) const PIN_MARK: &str = " ★";
pub(super) const PIN_WIDTH: usize = 2;

/// Presentation of [`SessionStatus`] in the session list (the vocabulary
/// itself lives in the protocol layer — one definition, shared with the event
/// projections).
impl SessionStatus {
    /// 状态图标：inactive/idle/working 为实心圆点，waiting 为非 emoji 的 `?`。
    pub fn icon(self) -> &'static str {
        match self {
            Self::Waiting => "?",
            _ => "●",
        }
    }

    /// 状态图标颜色（走调色板槽位，不引命名 ANSI 色：主题改一次，这里跟着变）。
    ///
    /// Working = `warning`（忙），Waiting = `accent`（轮到你了），Idle = `text`，
    /// Inactive = `dim`（被逐出内存的会话）。
    pub fn color(self, palette: &ThemePalette) -> Color {
        match self {
            Self::Working => palette.warning,
            Self::Waiting => palette.accent,
            Self::Idle => palette.text,
            Self::Inactive => palette.dim,
        }
    }
}

/// Format an ISO 8601 timestamp (e.g. `2025-07-22T21:41:28.123`) into a compact
/// display string `07-22 21:41`. Returns empty string for unparseable input.
pub fn format_last_active(iso: &str) -> String {
    // Expect "YYYY-MM-DDTHH:MM:SS..." — extract MM-DD HH:MM.
    let s = iso.trim();
    if s.len() < 16 {
        return String::new();
    }
    let bytes = s.as_bytes();
    // Validate minimal structure: digits and separators at expected positions.
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' {
        return String::new();
    }
    // 前面四个分隔符检查顺带保证了 5/8/11/14 都是字符边界，16 没有：一个横跨
    // 14..16 的非 ASCII 字符（手改 metadata / 第三方网关写脏的 last_interaction）
    // 会让字节切片 panic，而这里的契约是「看不懂就给空串」。
    let Some(minute) = s.get(14..16) else {
        return String::new();
    };
    // "MM-DD HH:MM"
    format!("{}-{} {}:{}", &s[5..7], &s[8..10], &s[11..13], minute)
}

/// crew 徽章：`▸N`（dim，全部后代）/ `?k`（accent，后代在等你）/ `●k`（warning，后代在
/// 干活）。三个计数为 0 的标记**不出现**——0 是常态，画出来只会把每一行都塞满。
///
/// `budget` 是徽章块可用的列（调用方已扣掉后面那格空气）：极窄面板上按声明序**截尾**
/// （先丢 `●`、再丢 `?`），绝不把星标或时间标签顶出行外。返回（span 列表，显示宽度）。
pub(super) fn crew_badges(
    crew: Crew,
    palette: &ThemePalette,
    base: Style,
    budget: usize,
) -> (Vec<Span<'static>>, usize) {
    let items = [
        (crew.descendants, "▸", palette.dim),
        (crew.waiting, "?", palette.accent),
        (crew.working, "●", palette.warning),
    ];
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut width = 0usize;
    for (count, glyph, ink) in items {
        if count == 0 {
            continue;
        }
        let text = format!("{glyph}{count}");
        let item_w = UnicodeWidthStr::width(text.as_str());
        let sep_w = usize::from(!spans.is_empty());
        if width + sep_w + item_w > budget {
            break;
        }
        if sep_w == 1 {
            spans.push(Span::styled(" ", base));
        }
        width += sep_w + item_w;
        spans.push(Span::styled(text, Style::default().fg(ink).patch(base)));
    }
    (spans, width)
}
