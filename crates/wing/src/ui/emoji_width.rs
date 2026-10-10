//! Emoji widths on the wire: a cell's declared diff width has to equal the
//! columns the terminal advances for its symbol.
//!
//! `BufferDiff` hands `CrosstermBackend` the cells to write, and the backend
//! skips its `MoveTo` whenever the next cell sits at `last.x + 1` — i.e. it
//! assumes the symbol it just printed advanced the cursor by exactly one
//! column. What keeps that assumption true is the *declared* width: `BufferDiff`
//! skips the cells a wide symbol covers (`self.pos += width - 1`), so a cell
//! emitted right after a wide one is never adjacent.
//!
//! `U+FE0F` (VS16, "emoji presentation") is the one symbol where the two sides
//! disagree. A cell holding a base character + `U+FE0F` measures 2 columns
//! (`unicode-width` resolves the presentation sequences, and `Buffer::set_stringn`
//! already reserves 2 columns when it lays the grapheme out), and `BufferDiff`
//! takes its wide branch — but it *also* re-emits the cells covered by the
//! symbol, as a workaround for terminals that fail to clear the trailing cell of
//! an emoji. The backend sees that trailing cell at `x + 1`, believes the cursor
//! is already there, and prints one column too far right; every later cell of
//! that row inherits the offset, because the backend's model of the cursor is
//! off by one from there on. A shifted write that lands on half of a CJK glyph
//! makes the terminal drop the whole glyph — the "missing character" the user
//! sees, while the buffer (and the copy) stayed correct. The damage survives
//! until the next full repaint (focus regain / resize), because a cell that
//! stops changing is never re-sent.
//!
//! The repair is one pass over the finished frame: pin such a cell to
//! `CellDiffOption::ForcedWidth(width)`, the mechanism the OSC8 injection
//! already uses (see [`crate::ui::chat_view::link`]) — with the same rule
//! attached to it, since the forced value has to be the width the *terminal*
//! renders. A pinned cell takes `BufferDiff`'s ordinary wide branch: the cells
//! it covers are skipped like a CJK glyph's, so nothing is ever written at
//! `last.x + 1` after a wide symbol, and the row stays in step.
//!
//! What it costs: this is exactly upstream's VS16 workaround, switched off for
//! these cells — they behave like a CJK glyph from here on, which is also what
//! the layout (2 reserved columns) and the copy path already assume. The
//! trade-off and its exit condition are recorded in the issue (#181); the
//! upstream fix for the backend's adjacency check (ratatui#2721) makes this pass
//! a candidate for removal once a release carries it.

use std::num::NonZeroU16;

use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::buffer::CellDiffOption;
use ratatui::buffer::CellWidth;

use crate::render::markdown::strip_osc8;

/// The variation selector that asks for the emoji presentation (`U+FE0F`).
const EMOJI_PRESENTATION: char = '\u{FE0F}';

/// Pin the declared width of every wide emoji-presentation cell of `buf`.
///
/// Returns how many cells were pinned (0 for a frame without one — the pass is
/// targeted, not a general width normalisation; see the module docs).
///
/// Runs after every widget has written its cells and before the buffer is
/// diffed against the previous frame, so the diff that reaches the backend
/// describes what the terminal will do.
pub fn pin(buf: &mut Buffer) -> usize {
    let mut pinned = 0;
    for cell in &mut buf.content {
        if pin_cell(cell) {
            pinned += 1;
        }
    }
    pinned
}

/// [`pin`] for one cell — `true` when its width was pinned.
///
/// A cell another pass owns keeps its directive: `Skip` is never written at
/// all (an image anchor's cells), `ForcedWidth` already carries the width the
/// terminal renders (the OSC8 injection, the kitty placeholders), and
/// `AlwaysUpdate` has to stay unconditional — pinning it would silently turn it
/// into an emit-only-on-change cell, trading one wrong behaviour for another.
#[allow(deprecated)] // `Cell::skip` is legacy, but it is part of the diff's rule
fn pin_cell(cell: &mut Cell) -> bool {
    if !matches!(cell.diff_option, CellDiffOption::None) || cell.skip {
        return false;
    }
    // The escape sequences of a hyperlink ride in the symbol; they are not
    // columns, and measuring them would make this cell look far wider than it
    // is. (`BufferDiff` reads the symbol too, which is why the injection pins
    // its width — a cell that carries a sequence without a pin never gets here.)
    let symbol = strip_osc8(cell.symbol());
    if !symbol.contains(EMOJI_PRESENTATION) {
        return false;
    }
    // Only the wide ones: a narrow symbol advances the terminal by one column
    // whatever the diff believes, which is precisely what the backend's
    // shortcut assumes, so there is nothing to pin. The width comes from
    // ratatui's own measure (`CellWidth`), so pinning changes which branch the
    // diff takes and never the number it computes.
    let Some(width) = NonZeroU16::new(symbol.cell_width()).filter(|w| w.get() > 1) else {
        return false;
    };
    cell.set_diff_option(CellDiffOption::ForcedWidth(width));
    true
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    use super::*;
    use crate::render::markdown::osc8_close;
    use crate::render::markdown::osc8_open;
    use crate::render::markdown::symbol_width;

    /// A one-row buffer holding `text`, laid out the way a widget would.
    fn row(text: &str, width: u16) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        buf.set_stringn(0, 0, text, width as usize, Style::default());
        buf
    }

    /// The emoji of [`row`]'s first case, as the terminal renders it: 2 columns.
    const EMOJI: &str = "⚠️";

    #[test]
    fn a_wide_emoji_presentation_cell_pins_the_terminal_width() {
        let mut buf = row(&format!("A{EMOJI}"), 10);
        assert_eq!(symbol_width(EMOJI), 2, "the premise: VS16 measures two");
        assert_eq!(pin(&mut buf), 1);
        assert_eq!(
            buf[(1, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap()),
            "the emoji's declared width is the columns the terminal advances"
        );
        // Its filler column is untouched: the pin is what makes the diff skip
        // it, exactly as it skips a CJK glyph's.
        assert_eq!(buf[(2, 0)].diff_option, CellDiffOption::None);
        assert_eq!(buf[(0, 0)].diff_option, CellDiffOption::None, "the `A`");
    }

    #[test]
    fn plain_wide_and_narrow_text_is_left_alone() {
        // The pass is not a general width normalisation: CJK already takes the
        // branch that skips its filler, and narrow cells never need one.
        let mut buf = row("世界 ab ➡", 20);
        assert_eq!(pin(&mut buf), 0);
        assert!(
            buf.content
                .iter()
                .all(|cell| cell.diff_option == CellDiffOption::None),
            "nothing was pinned"
        );
        // A `U+FE0E` (text presentation) selector is not the emoji one.
        let mut text_selector = row("A⚠\u{FE0E}B", 10);
        assert_eq!(pin(&mut text_selector), 0);
    }

    #[test]
    fn a_narrow_emoji_presentation_cell_is_not_pinned() {
        // A sequence unicode-width keeps at one column advances the terminal by
        // one column too, so the backend's shortcut is right about it.
        let mut buf = row("A\u{FE0F}B", 10);
        assert_eq!(
            symbol_width("\u{FE0F}"),
            0,
            "a lone selector measures nothing"
        );
        assert_eq!(pin(&mut buf), 0);
    }

    #[test]
    fn a_hyperlink_wrapped_emoji_is_measured_without_its_sequences() {
        // The OSC8 injection pins its cells itself; the pass must neither
        // measure the URL (which would claim tens of columns) nor overwrite a
        // directive another pass owns.
        let injected = CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap());
        let mut linked = row(&format!("A{EMOJI}"), 10);
        linked[(1, 0)]
            .set_symbol(&format!(
                "{}⚠{}",
                osc8_open("https://example.com"),
                osc8_close()
            ))
            .set_diff_option(injected);
        assert_eq!(pin(&mut linked), 0);
        assert_eq!(linked[(1, 0)].diff_option, injected, "the link's own pin");
    }

    #[test]
    fn a_cell_another_pass_owns_keeps_its_directive() {
        // Three emojis, one column of air apart: their head cells sit at 0, 3
        // and 6 (a 2-column symbol per cell, its filler right after).
        let mut buf = row(&format!("{EMOJI} {EMOJI} {EMOJI}"), 12);
        assert_eq!(buf[(3, 0)].symbol(), EMOJI, "the second head");
        buf[(0, 0)].set_diff_option(CellDiffOption::Skip);
        buf[(3, 0)].set_diff_option(CellDiffOption::AlwaysUpdate);
        buf[(6, 0)].set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap()));

        assert_eq!(pin(&mut buf), 0, "all three carried a directive already");
        assert_eq!(buf[(0, 0)].diff_option, CellDiffOption::Skip);
        assert_eq!(buf[(3, 0)].diff_option, CellDiffOption::AlwaysUpdate);
        assert_eq!(
            buf[(6, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap())
        );
    }

    #[test]
    fn the_pass_is_idempotent_and_counts_every_pinned_cell() {
        let mut buf = row(&format!("{EMOJI} ok {EMOJI}"), 12);
        assert_eq!(pin(&mut buf), 2);
        let pinned = buf.content.clone();
        assert_eq!(pin(&mut buf), 0, "a second run has nothing left to do");
        assert_eq!(buf.content.len(), pinned.len());
        for (after, before) in buf.content.iter().zip(pinned.iter()) {
            assert_eq!(after.symbol(), before.symbol());
            assert_eq!(after.diff_option, before.diff_option);
        }
    }

    #[test]
    fn the_buffer_is_not_re_laid_out() {
        // The pass touches the diff directive only: the symbols, the styles and
        // the columns of every cell are what the widgets wrote.
        let text = format!("A{EMOJI}世界");
        let mut buf = row(&text, 12);
        let before = buf.content.clone();
        assert_eq!(pin(&mut buf), 1);
        for (after, before) in buf.content.iter().zip(before.iter()) {
            assert_eq!(after.symbol(), before.symbol());
            assert_eq!(after.fg, before.fg);
            assert_eq!(after.bg, before.bg);
            assert_eq!(after.modifier, before.modifier);
        }
    }
}
