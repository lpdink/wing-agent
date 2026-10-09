//! Shared table layout engine: column measurement, classification and width
//! allocation.
//!
//! One copy of the maths, consumed by the TUI markdown table renderer
//! ([`crate::render::markdown::tables`]) and the CLI plain-text table renderer
//! ([`plain`], used by `wing ps` / `wing tools`).
//!
//! Everything here works on numbers and string measurements only: no
//! `ratatui` styles, no assumption about which frontend is drawing. The width
//! policy (which column gives up width first, which one is preserved last) is
//! a product decision that lives in [`compute_column_widths`] — sharing it is
//! what keeps the two frontends from growing two different looks.

pub mod plain;

use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// Hard minimum column width.
pub const MIN_COLUMN_WIDTH: usize = 3;
/// Soft readable floor for narrative / token-heavy columns.
pub const PREFERRED_FLOOR: usize = 16;
/// A whitespace token at least this wide marks its column as token-heavy.
pub const LONG_TOKEN_WIDTH: usize = 20;

/// Classification of a table column for width-allocation priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    /// Long-form prose content.
    Narrative,
    /// Paths, URLs, hashes — long unbroken tokens.
    TokenHeavy,
    /// Short values such as counts or status labels.
    Compact,
}

/// Per-column measurements used for width allocation.
pub struct ColumnMetrics {
    /// Widest cell content (display width) in this column.
    pub max_width: usize,
    /// Widest whitespace token in the header cell.
    pub header_token_width: usize,
    /// Widest whitespace token across body cells.
    pub body_token_width: usize,
    pub kind: ColumnKind,
}

/// Classify a column from its cell statistics.
///
/// The thresholds are a product decision shared by every frontend: token-heavy
/// columns surrender width first, narrative prose next, compact columns last.
pub fn classify_column(
    avg_words_per_cell: f64,
    avg_cell_width: f64,
    long_body_token_count: usize,
    body_token_count: usize,
) -> ColumnKind {
    if long_body_token_count > 0
        && long_body_token_count >= body_token_count.saturating_sub(long_body_token_count)
    {
        ColumnKind::TokenHeavy
    } else if avg_words_per_cell >= 4.0 || avg_cell_width >= 28.0 {
        ColumnKind::Narrative
    } else {
        ColumnKind::Compact
    }
}

/// Allocate column content widths so the table fits within `content_budget`.
///
/// Each column starts at its natural (max cell content) width, then columns are
/// shrunk one character at a time until the total fits. Token-heavy columns
/// shrink before narrative prose; compact columns are preserved last. Always
/// returns widths whose sum is `<= content_budget` (when a budget is given).
pub fn compute_column_widths(
    metrics: &[ColumnMetrics],
    content_budget: Option<usize>,
) -> Vec<usize> {
    let col_count = metrics.len();
    let mut widths: Vec<usize> = metrics
        .iter()
        .map(|m| m.max_width.max(MIN_COLUMN_WIDTH))
        .collect();

    let Some(budget) = content_budget else {
        return widths;
    };
    if col_count == 0 {
        return widths;
    }

    // Degenerate budget: cannot even hold minimum-width columns. Split evenly.
    let min_total = col_count * MIN_COLUMN_WIDTH;
    if budget < min_total {
        let share = (budget / col_count).max(1);
        return vec![share; col_count];
    }

    // Preferred floors, relaxed in shrink-priority order until they fit.
    let mut floors: Vec<usize> = metrics
        .iter()
        .map(|m| preferred_column_floor(m, MIN_COLUMN_WIDTH))
        .collect();
    let mut floor_total: usize = floors.iter().sum();
    while floor_total > budget {
        let Some((idx, _)) = floors
            .iter()
            .enumerate()
            .filter(|(_, floor)| **floor > MIN_COLUMN_WIDTH)
            .min_by_key(|(idx, floor)| {
                (
                    shrink_priority(metrics[*idx].kind),
                    usize::MAX.saturating_sub(**floor),
                )
            })
        else {
            break;
        };
        floors[idx] -= 1;
        floor_total -= 1;
    }

    // Shrink columns one char at a time until the total fits the budget.
    let mut total: usize = widths.iter().sum();
    while total > budget {
        let Some(idx) = next_column_to_shrink(&widths, &floors, metrics) else {
            break;
        };
        widths[idx] -= 1;
        total -= 1;
    }

    widths
}

/// Preferred minimum width for a column before the shrink loop runs.
///
/// Narrative and token-heavy columns keep a readable 16-cell soft floor; compact
/// columns floor at the wider of their header/body token widths (body capped at
/// 16). Clamped to `[min, max_width]`.
fn preferred_column_floor(metrics: &ColumnMetrics, min: usize) -> usize {
    let target = match metrics.kind {
        ColumnKind::Narrative | ColumnKind::TokenHeavy => PREFERRED_FLOOR,
        ColumnKind::Compact => metrics
            .header_token_width
            .max(metrics.body_token_width.min(PREFERRED_FLOOR)),
    };
    target.max(min).min(metrics.max_width.max(min))
}

/// Pick the next column to shrink by one character.
///
/// Priority: TokenHeavy before Narrative before Compact. Within the same kind,
/// the column with the most slack above its floor shrinks first so similarly
/// shaped columns stay balanced.
fn next_column_to_shrink(
    widths: &[usize],
    floors: &[usize],
    metrics: &[ColumnMetrics],
) -> Option<usize> {
    widths
        .iter()
        .enumerate()
        .filter(|(idx, width)| **width > floors[*idx])
        .min_by_key(|(idx, width)| {
            let slack = width.saturating_sub(floors[*idx]);
            (
                shrink_priority(metrics[*idx].kind),
                usize::MAX.saturating_sub(slack),
            )
        })
        .map(|(idx, _)| idx)
}

fn shrink_priority(kind: ColumnKind) -> usize {
    match kind {
        ColumnKind::TokenHeavy => 0,
        ColumnKind::Narrative => 1,
        ColumnKind::Compact => 2,
    }
}

/// Longest whitespace-delimited token width in `text` (CJK-aware).
pub fn longest_token_width(text: &str) -> usize {
    text.split_whitespace()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}

/// Split a string at a display width boundary (CJK-safe).
///
/// Returns `(head, tail)` where `head` fits within `max_width` display columns.
pub(crate) fn split_str_by_width(text: &str, max_width: usize) -> (&str, &str) {
    if max_width == 0 {
        return ("", text);
    }
    let mut width = 0;
    for (i, ch) in text.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max_width {
            return (&text[..i], &text[i..]);
        }
        width += cw;
    }
    (text, "")
}

// ============================================================
// Table skin — the box-drawing glyphs of one table look
// ============================================================

/// Glyphs of one horizontal rule line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleGlyphs {
    /// Left end (a corner or a T-junction against the frame).
    pub left: char,
    /// Junction where an inner column divider crosses the rule.
    pub junction: char,
    /// Right end.
    pub right: char,
    /// The rule's fill character (its stroke weight).
    pub fill: char,
}

/// Box-drawing skin shared by every table frontend.
///
/// The default skin is **三档重框** (heavy three-tier frame): the outer frame
/// and the header band are drawn heavy, the body grid light. Three tiers are
/// carried by *stroke weight alone* — every stroke shares one ink colour
/// (theme `border`), because weight is what a terminal renders reliably while
/// a DIM modifier is at the font's mercy.
///
/// Sharing one skin is what keeps the TUI markdown tables and the CLI tables
/// (`wing ps` / `wing tools`) from drifting into two different looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableSkin {
    pub top: RuleGlyphs,
    pub header_sep: RuleGlyphs,
    pub body_sep: RuleGlyphs,
    pub bottom: RuleGlyphs,
    /// Outer frame vertical — heavy, so the table reads as wrapped.
    pub outer_v: char,
    /// Column divider inside the header row — heavy: the header band reads as
    /// part of the frame (it is the table's cap).
    pub header_v: char,
    /// Column divider inside body rows — light, so data cells breathe.
    pub inner_v: char,
}

impl TableSkin {
    /// The one skin: heavy frame, heavy header band, light body grid.
    ///
    /// Junction glyphs follow the weight transition (Unicode pairs every
    /// light/heavy combination): the header separator drops from the heavy
    /// header row to the light body (`╇`), body separators hang off the heavy
    /// frame (`┠` / `┨`), the bottom border meets light body verticals (`┷`).
    pub const fn framed() -> Self {
        Self {
            top: RuleGlyphs {
                left: '┏',
                junction: '┳',
                right: '┓',
                fill: '━',
            },
            header_sep: RuleGlyphs {
                left: '┣',
                junction: '╇',
                right: '┫',
                fill: '━',
            },
            body_sep: RuleGlyphs {
                left: '┠',
                junction: '┼',
                right: '┨',
                fill: '─',
            },
            bottom: RuleGlyphs {
                left: '┗',
                junction: '┷',
                right: '┛',
                fill: '━',
            },
            outer_v: '┃',
            header_v: '┃',
            inner_v: '│',
        }
    }
}

/// Fixed width cost of the frame for `col_count` columns: 2 outer verticals,
/// 2 padding cells per column, 1 inner divider between adjacent columns.
///
/// The column *content* widths share `available_width - frame_overhead(n)`.
pub fn frame_overhead(col_count: usize) -> usize {
    col_count * 3 + 1
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn compact_metrics(max_width: usize) -> ColumnMetrics {
        ColumnMetrics {
            max_width,
            header_token_width: 3,
            body_token_width: 5,
            kind: ColumnKind::Compact,
        }
    }

    #[test]
    fn test_split_str_by_width_ascii() {
        assert_eq!(split_str_by_width("hello world", 5), ("hello", " world"));
        assert_eq!(split_str_by_width("hello", 10), ("hello", ""));
        assert_eq!(split_str_by_width("abc", 0), ("", "abc"));
    }

    #[test]
    fn test_split_str_by_width_cjk() {
        // Each CJK char = 2 columns.
        assert_eq!(split_str_by_width("你好世界", 4), ("你好", "世界"));
        assert_eq!(split_str_by_width("你好世界", 3), ("你", "好世界"));
        assert_eq!(split_str_by_width("a你b", 3), ("a你", "b"));
    }

    #[test]
    fn test_longest_token_width() {
        assert_eq!(longest_token_width("a bb ccc"), 3);
        assert_eq!(longest_token_width(""), 0);
        assert_eq!(longest_token_width("你好 ab"), 4); // 你好 = 4 cols
    }

    #[test]
    fn test_classify_token_heavy() {
        // A column of long paths classifies as TokenHeavy.
        assert_eq!(
            classify_column(1.0, 30.0, 2, 2),
            ColumnKind::TokenHeavy,
            "two long tokens out of two"
        );
    }

    #[test]
    fn test_classify_compact() {
        assert_eq!(classify_column(1.0, 2.0, 0, 2), ColumnKind::Compact);
    }

    #[test]
    fn test_classify_narrative() {
        assert_eq!(classify_column(6.0, 20.0, 0, 6), ColumnKind::Narrative);
        // A single long cell with many words is narrative too.
        assert_eq!(classify_column(1.0, 30.0, 0, 1), ColumnKind::Narrative);
    }

    #[test]
    fn test_compute_widths_no_budget_keeps_natural() {
        let metrics = vec![compact_metrics(10), compact_metrics(20)];
        let widths = compute_column_widths(&metrics, None);
        assert_eq!(widths, vec![10, 20]);
    }

    #[test]
    fn test_compute_widths_fits_budget() {
        let metrics = vec![
            compact_metrics(10),
            ColumnMetrics {
                max_width: 80,
                header_token_width: 3,
                body_token_width: 60,
                kind: ColumnKind::TokenHeavy,
            },
            ColumnMetrics {
                max_width: 40,
                header_token_width: 3,
                body_token_width: 30,
                kind: ColumnKind::Narrative,
            },
        ];
        let widths = compute_column_widths(&metrics, Some(60));
        let total: usize = widths.iter().sum();
        assert!(total <= 60, "total {total} exceeds budget 60: {widths:?}");
        assert!(widths.iter().all(|&w| w >= 1));
    }

    #[test]
    fn test_compute_widths_token_heavy_shrinks_first() {
        // TokenHeavy column should give up width before the Narrative column.
        let metrics = vec![
            ColumnMetrics {
                max_width: 60,
                header_token_width: 3,
                body_token_width: 50,
                kind: ColumnKind::TokenHeavy,
            },
            ColumnMetrics {
                max_width: 60,
                header_token_width: 3,
                body_token_width: 30,
                kind: ColumnKind::Narrative,
            },
        ];
        let widths = compute_column_widths(&metrics, Some(60));
        assert!(
            widths[1] >= widths[0],
            "narrative ({}) should retain at least as much width as token-heavy ({}): {widths:?}",
            widths[1],
            widths[0]
        );
    }

    #[test]
    fn test_compute_widths_degenerate_budget() {
        let metrics = vec![
            compact_metrics(10),
            compact_metrics(10),
            compact_metrics(10),
        ];
        // Budget too small for 3 * MIN_COLUMN_WIDTH.
        let widths = compute_column_widths(&metrics, Some(6));
        assert_eq!(widths.len(), 3);
        assert!(widths.iter().all(|&w| w >= 1));
    }

    #[test]
    fn frame_overhead_matches_the_assembly_math() {
        // 2 outer verticals + 2 padding cells per column + 1 divider per
        // adjacent pair: `┃ a │ b ┃` is 4 + 3 for one divider.
        assert_eq!(frame_overhead(1), 4);
        assert_eq!(frame_overhead(2), 7);
        assert_eq!(frame_overhead(3), 10);
    }

    /// Every skin glyph must be exactly one terminal cell wide — a glyph from
    /// an ambiguous-width or emoji table would desynchronise every row after
    /// the first one (the renderers count columns, not chars).
    #[test]
    fn skin_glyphs_are_single_width() {
        let skin = TableSkin::framed();
        for rule in [skin.top, skin.header_sep, skin.body_sep, skin.bottom] {
            for ch in [rule.left, rule.junction, rule.right, rule.fill] {
                assert_eq!(UnicodeWidthChar::width(ch), Some(1), "{ch:?}");
            }
        }
        for ch in [skin.outer_v, skin.header_v, skin.inner_v] {
            assert_eq!(UnicodeWidthChar::width(ch), Some(1), "{ch:?}");
        }
    }
}
