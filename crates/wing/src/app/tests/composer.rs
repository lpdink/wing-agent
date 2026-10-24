//! The composer card's rendering contract, asserted on the frame the app
//! really drew (`TestBackend`).
//!
//! These are the tests that say what the bottom of the screen *looks like*:
//! the frame around the draft, the two rails inside it, and the two rows the
//! card used to spend above itself (the working indicator and the info
//! separator) now being part of it.

use std::time::Instant;

use super::support::*;
use crate::app::App;
use crate::app::MouseOutcome;
use crate::ui::chat_view::ChatCell;

/// Draw the app and return `(frame, the composer block it recorded)`.
fn draw_frame(
    app: &mut App,
    width: u16,
    height: u16,
) -> (ratatui::buffer::Buffer, ratatui::layout::Rect) {
    let mut terminal = test_terminal(width, height);
    draw(app, &mut terminal);
    let composer = app.input.rendered_area();
    (terminal.backend().buffer().clone(), composer)
}

/// Spaces between a row's content and its right border.
fn pad(width: usize, content: &str) -> usize {
    width - content.chars().count() - 1
}

/// One row of the **card** as text (the float's margins excluded).
fn card_row(buf: &ratatui::buffer::Buffer, composer: ratatui::layout::Rect, row: u16) -> String {
    (composer.x..composer.right())
        .map(|x| buf[(x, row)].symbol())
        .collect()
}

#[test]
fn test_the_card_frames_the_draft() {
    let mut app = app_with_draft("hello world");
    let (buf, composer) = draw_frame(&mut app, 40, 10);

    let width = composer.width as usize;
    assert_eq!(
        card_row(&buf, composer, composer.y),
        format!("╭{}╮", "─".repeat(width - 2)),
        "a drafted card's top rail is a plain rule:\n{}",
        frame_text(&buf)
    );
    assert_eq!(
        card_row(&buf, composer, composer.y + 1),
        format!(
            "│ ❯ hello world{}│",
            " ".repeat(pad(width, "│ ❯ hello world"))
        ),
        "the draft sits inside the frame, behind the prompt glyph:\n{}",
        frame_text(&buf)
    );
    assert_eq!(
        card_row(&buf, composer, composer.bottom() - 1),
        format!("╰{}╯", "─".repeat(width - 2)),
        "the meta rail closes the card (nothing to report here):\n{}",
        frame_text(&buf)
    );
    // The card floats: the margins outside it are never touched.
    for y in composer.y..composer.bottom() {
        assert_eq!(buf[(composer.x - 1, y)].symbol(), " ", "left margin at {y}");
        assert_eq!(
            buf[(composer.right(), y)].symbol(),
            " ",
            "right margin at {y}"
        );
    }
}

#[test]
fn test_the_card_grows_with_the_draft_and_keeps_the_frame() {
    let mut app = app_with_draft("one\ntwo\nthree");
    let (buf, composer) = draw_frame(&mut app, 40, 12);
    assert_eq!(composer.height, 3 + 2, "three text rows inside the frame");

    // The prompt glyph opens the draft; the following rows line up with the
    // text it introduced, and every row carries both vertical borders — the
    // card is a box, not a rule pair.
    let width = composer.width as usize;
    assert_eq!(
        card_row(&buf, composer, composer.y + 1),
        format!("│ ❯ one{}│", " ".repeat(pad(width, "│ ❯ one")))
    );
    assert_eq!(
        card_row(&buf, composer, composer.y + 2),
        format!("│   two{}│", " ".repeat(pad(width, "│   two")))
    );
    assert_eq!(
        card_row(&buf, composer, composer.y + 3),
        format!("│   three{}│", " ".repeat(pad(width, "│   three")))
    );
    for y in composer.y..composer.bottom() {
        let row = card_row(&buf, composer, y);
        let (left, right) = (row.chars().next(), row.chars().last());
        assert!(
            matches!(left, Some('╭' | '│' | '╰')) && matches!(right, Some('╮' | '│' | '╯')),
            "row {y} must be closed on both sides: {row}"
        );
    }
}

#[test]
fn test_the_activity_rail_replaces_the_working_row() {
    let mut app = app_with_draft("queued while the agent works");
    app.turn.working = true;
    app.turn.started_at = Some(Instant::now());
    let (buf, composer) = draw_frame(&mut app, 60, 12);
    let top = card_row(&buf, composer, composer.y);

    assert!(
        top.starts_with("╭─ ⠋ Working..."),
        "the spinner opens the top rail:\n{}",
        frame_text(&buf)
    );
    assert!(
        top.contains("Esc to interrupt") && top.ends_with('╮'),
        "the interrupt hint rides the rail, the dashes fill the rest:\n{}",
        frame_text(&buf)
    );
    // A running turn costs no row of its own: the card is still frame + text.
    assert_eq!(
        composer.height, 3,
        "one text row, two borders — the indicator is not a row"
    );
    assert_eq!(
        app.geometry.chat_band().bottom(),
        composer.y,
        "the band ends where the card starts"
    );
}

#[test]
fn test_the_meta_rail_carries_the_session_read_out() {
    let mut app = app_with_draft("draft");
    app.status.workdir = Some("/tmp/ws".into());
    app.chat.push(ChatCell::AssistantMessage(
        (0..40)
            .map(|i| format!("msg {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    let (buf, composer) = draw_frame(&mut app, 80, 12);
    let bottom = card_row(&buf, composer, composer.bottom() - 1);

    assert!(
        bottom.starts_with("╰─ /tmp/ws"),
        "the workdir leads the meta rail:\n{}",
        frame_text(&buf)
    );
    assert!(
        bottom.contains("100%"),
        "the scroll read-out settles on the right:\n{}",
        frame_text(&buf)
    );
}

#[test]
fn test_the_card_keeps_the_terminal_background() {
    let palette = crate::config::ThemePalette::default();

    // The card owns no surface: with a draft, frame, rails and the draft between
    // them all stay on whatever background the terminal has (the float's margins
    // included).
    let mut app = app_with_draft("hello world");
    let (buf, composer) = draw_frame(&mut app, 40, 10);
    for y in composer.y..composer.bottom() {
        for x in composer.x - 1..composer.right() + 1 {
            assert_eq!(
                buf[(x, y)].bg,
                ratatui::style::Color::Reset,
                "({x},{y}) must not be filled"
            );
        }
    }

    // The one fill the card may carry is a keycap on a rail — `ui::chip`'s quiet
    // band under the empty card's invite, on the top border and nowhere else. The
    // verb beside the key lies on the terminal's own background: the band is the
    // *key*, not the sentence. (The float's margins are clear from the loop above.)
    let mut app = app_with_draft("");
    let (buf, composer) = draw_frame(&mut app, 40, 10);
    // Whatever `ui::chip` picks for the quiet band *is* the band: restating the
    // slot here would let the two drift, so this only holds it to one face.
    let mut band: Option<ratatui::style::Color> = None;
    let mut filled: Vec<u16> = Vec::new();
    for y in composer.y..composer.bottom() {
        for x in composer.x..composer.right() {
            let bg = buf[(x, y)].bg;
            if bg == ratatui::style::Color::Reset {
                continue;
            }
            assert_ne!(bg, palette.accent, "a keycap, not a badge ({x},{y})");
            assert_eq!(
                *band.get_or_insert(bg),
                bg,
                "one face for the whole key ({x},{y})"
            );
            assert_eq!(y, composer.y, "a keycap belongs on the rail ({x},{y})");
            filled.push(x);
        }
    }
    assert!(!filled.is_empty(), "the invite is drawn");
    let (first, last) = (filled[0], filled[filled.len() - 1]);
    assert_eq!(
        filled.len() as u16,
        last - first + 1,
        "one block, not a dotted run: {filled:?}"
    );
    // One block, and it stops where the key does: no dash, no verb inside it.
    let cells: Vec<char> = card_row(&buf, composer, composer.y).chars().collect();
    let rel = |x: u16| (x - composer.x) as usize;
    let band: String = cells[rel(first)..=rel(last)].iter().collect();
    assert!(!band.contains('─'), "the band is the key: {band:?}");
    assert!(!band.contains('发'), "the verb is not a key: {band:?}");
    let after: String = cells[rel(last) + 1..].iter().take(6).collect();
    assert!(after.contains('发'), "the verb follows the key: {after:?}");
}

/// The outline's register: `tool_result` while the card waits (a box, not a
/// hairline), one notch toward the accent while a draft waits in it, quiet again
/// when another layer takes the keys. The exact ink is pinned in
/// `ui::input_area::chrome`.
#[test]
fn test_the_edge_follows_the_draft_and_the_held_keyboard() {
    let palette = crate::config::ThemePalette::default();
    let caret = |app: &App, buf: &ratatui::buffer::Buffer| {
        let (text_x, text_y, _) = composer_text(app);
        buf[(text_x - 2, text_y)].fg
    };
    let edges = |buf: &ratatui::buffer::Buffer, composer: ratatui::layout::Rect| {
        let mut inks = Vec::new();
        for y in composer.y..composer.bottom() {
            inks.push(buf[(composer.x, y)].fg);
            inks.push(buf[(composer.right() - 1, y)].fg);
        }
        inks
    };

    // Empty: nothing typed yet, so the frame is what says "there is a box to
    // type into" — and the caret belongs to that box, not to a draft that is
    // not there (one register for an empty card: the invitation must not read
    // as content).
    let mut app = app_with_draft("");
    let (buf, composer) = draw_frame(&mut app, 40, 10);
    assert_eq!(
        caret(&app, &buf),
        palette.tool_result,
        "❯ on an empty draft"
    );
    for ink in edges(&buf, composer) {
        assert_eq!(ink, palette.tool_result, "an empty card's edge is its box");
    }

    // A draft is waiting: the boundary turns toward the accent (never as far as
    // the accent itself — the caret and the commands keep the saturated one).
    let mut app = app_with_draft("hello world");
    let (buf, composer) = draw_frame(&mut app, 40, 10);
    assert_eq!(caret(&app, &buf), palette.accent, "❯ with a draft");
    for ink in edges(&buf, composer) {
        assert_ne!(ink, palette.dim, "a drafted card's edge steps up");
        assert_ne!(ink, palette.accent, "and it is not the full accent");
        assert_ne!(
            ink, palette.tool_result,
            "and it is the hue shift that says so"
        );
    }

    // An ask panel holds the keys: the card ghosts as a whole, boundary included.
    app.ask_panels
        .push_back(required_choice_panel("call-1", &["y", "n"]));
    let (buf, composer) = draw_frame(&mut app, 40, 10);
    assert!(app.composer_typing_blocked(), "the panel takes the keys");
    for ink in edges(&buf, composer) {
        assert_eq!(ink, palette.dim, "a ghosted card promises nothing");
    }
}

/// 空卡的墨只有一档：框、`❯`、占位句、`Enter` 键帽都在 `tool_result` —— 邀请靠**形**说
/// （一张框 + 一枚键帽 + 一个起点），不靠亮度说。正文墨（`text`）只在有草稿的卡上出现，
/// 于是"空"与"有内容"在**墨**上就分得开（这是真机上"占位像正文"的直接反面）。
///
/// 亮度用**对纸面的对比度**量（`ui::chip` 的判据，明暗两套各自校准），所以这条断言
/// 在两套 preset 上都成立 —— 裸亮度会把亮色预设的深墨当成"安静"。
#[test]
fn the_empty_card_is_quiet_and_a_draft_is_loud() {
    use crate::config::ColorPreset;
    use crate::config::colors::parse_color;
    use crate::ui::chip;
    use crate::ui::html::page_colors;
    use crate::ui::shimmer::to_rgb;
    use ratatui::style::Color;

    /// 一张卡（给定 preset 与草稿）：帧、卡片矩形、文字区原点、调色板。
    fn card(
        preset: ColorPreset,
        draft: &str,
    ) -> (
        ratatui::buffer::Buffer,
        ratatui::layout::Rect,
        (u16, u16),
        crate::config::ThemePalette,
    ) {
        // The palette is resolved once, at construction: the frame has to be
        // built with the preset, not recoloured after it.
        let mut config = crate::config::AppConfig::default();
        config.colors.preset = preset;
        let mut app = App::new("test-session".into(), config, None);
        app.clear_welcome();
        app.status.workdir = Some("/tmp/ws".into());
        app.input.set_text(draft);
        let (buf, composer) = draw_frame(&mut app, 80, 10);
        let (text_x, text_y, _) = composer_text(&app);
        let palette = app.palette();
        (buf, composer, (text_x, text_y), palette)
    }

    fn paper(palette: &crate::config::ThemePalette) -> crate::ui::shimmer::Rgb {
        let (background, _) = page_colors(palette);
        to_rgb(parse_color(background).expect("the page colour parses"))
    }

    /// 卡内最亮的一格：(对纸面的对比度, 它的墨)。
    fn loudest(
        buf: &ratatui::buffer::Buffer,
        composer: ratatui::layout::Rect,
        palette: &crate::config::ThemePalette,
    ) -> (f32, Color) {
        let paper = paper(palette);
        let mut loudest = (0.0f32, Color::Reset);
        for y in composer.y..composer.bottom() {
            for x in composer.x..composer.right() {
                let cell = &buf[(x, y)];
                if cell.symbol() == " " {
                    continue;
                }
                let level = chip::contrast(to_rgb(cell.fg), paper);
                if level > loudest.0 {
                    loudest = (level, cell.fg);
                }
            }
        }
        loudest
    }

    for preset in [ColorPreset::Dark, ColorPreset::Light] {
        let (empty, card_rect, (text_x, text_y), palette) = card(preset, "");
        let (quiet, quiet_ink) = loudest(&empty, card_rect, &palette);
        let (drafted, drafted_rect, _, _) = card(preset, "hello world");
        let (lit, lit_ink) = loudest(&drafted, drafted_rect, &palette);

        assert_eq!(
            quiet_ink, palette.tool_result,
            "{preset:?}: 空卡最亮的字是提示那一档"
        );
        assert_eq!(lit_ink, palette.text, "{preset:?}: 有草稿时最亮的是正文");
        // 数值：暗色 6.07 对 15.92（2.6×）、亮色 6.39 对 15.80（2.5×）。卡 2 倍：
        // 比这更松的判据会放过「占位挪到 `thinking`（暗色 8.61）」那一类回归。
        assert!(
            quiet * 2.0 < lit,
            "{preset:?}: 空卡 {quiet:.2}:1 与草稿 {lit:.2}:1 必须分得开"
        );

        // 逐格：空卡里没有一格是正文墨或强调墨。
        for y in card_rect.y..card_rect.bottom() {
            for x in card_rect.x..card_rect.right() {
                let cell = &empty[(x, y)];
                if cell.symbol() == " " {
                    continue;
                }
                assert_ne!(
                    cell.fg, palette.text,
                    "{preset:?}: 空卡不该有正文墨 ({x},{y})"
                );
                assert_ne!(
                    cell.fg, palette.accent,
                    "{preset:?}: 空卡不该有强调墨 ({x},{y})"
                );
            }
        }
        assert_eq!(
            empty[(text_x, text_y)].fg,
            palette.tool_result,
            "{preset:?}: 占位句是提示"
        );
        assert_eq!(
            empty[(text_x - 2, text_y)].fg,
            palette.tool_result,
            "{preset:?}: `❯` 与占位同档"
        );
    }
}

/// "You can type here" outranks "you are here": the meta rail's path stays in
/// the quiet slot, under the card's own ink — an environment fact is not
/// something you can type.
///
/// The margin is not the same on both appearances: today `dim` sits at 3.23 vs
/// `tool_result` 6.07 on the dark page (1.9×) and 5.49 vs 6.39 on the light one
/// (1.16× — on a light paper `dim` is right under `tool_result`; that is the
/// palette's doing, not this card's). The assertions hold the floors (1.5× /
/// 1.1×); a palette change that eats the recorded ratio has to redraw this line.
#[test]
fn test_the_path_stays_under_the_cards_ink() {
    use crate::config::ColorPreset;
    use crate::config::colors::parse_color;
    use crate::ui::chip;
    use crate::ui::html::page_colors;
    use crate::ui::shimmer::to_rgb;

    for preset in [ColorPreset::Dark, ColorPreset::Light] {
        let mut config = crate::config::AppConfig::default();
        config.colors.preset = preset;
        let mut app = App::new("test-session".into(), config, None);
        app.clear_welcome();
        app.status.workdir = Some("/tmp/ws".into());
        let (buf, composer) = draw_frame(&mut app, 80, 10);
        let palette = app.palette();
        let (background, _) = page_colors(&palette);
        let paper = to_rgb(parse_color(background).expect("the page colour parses"));

        // The meta rail's leading item is the path: the first character on
        // the bottom row that is not part of the frame. Walk the row's
        // characters, not byte offsets into it — its box-drawing glyphs are
        // three bytes each.
        let bottom = card_row(&buf, composer, composer.bottom() - 1);
        let path_x = bottom
            .chars()
            .position(|c| !matches!(c, '╰' | '╯' | '│' | '─' | ' '))
            .expect("the workdir leads the rail") as u16;
        assert!(
            matches!(bottom.chars().nth(path_x as usize), Some('/' | '~')),
            "{preset:?}: the leading item is the path:\n{bottom}"
        );
        let path_ink = buf[(composer.x + path_x, composer.bottom() - 1)].fg;
        assert_eq!(path_ink, palette.dim, "{preset:?}: the path stays quiet");
        let path = chip::contrast(to_rgb(path_ink), paper);
        let frame = chip::contrast(to_rgb(palette.tool_result), paper);
        let margin = match preset {
            ColorPreset::Light => 1.1,
            _ => 1.5,
        };
        assert!(
            path * margin < frame,
            "{preset:?}: the path ({path:.2}:1) must stay under the card's own ink ({frame:.2}:1) by {margin}×"
        );
    }
}

#[test]
fn test_a_held_keyboard_ghosts_the_draft() {
    let mut app = app_with_draft("hello world");
    // An ask panel owns the keyboard: every key would go to it, so the card
    // must stop looking ready for one.
    app.ask_panels
        .push_back(required_choice_panel("call-1", &["y", "n"]));
    let (buf, _) = draw_frame(&mut app, 40, 10);
    let palette = crate::config::ThemePalette::default();
    let (text_x, text_y, _) = composer_text(&app);

    assert!(app.composer_typing_blocked(), "the panel takes the keys");
    assert_eq!(
        buf[(text_x, text_y)].fg,
        palette.dim,
        "the frozen draft is drawn as inactive text"
    );
    assert_eq!(
        buf[(text_x - 2, text_y)].fg,
        palette.dim,
        "❯ 一起变暗：光标不能一边说\"可以打字\"、一边把正文画成不可编辑"
    );
    // The panel closes: the draft is live again, in the message's own color,
    // and its glyph takes the accent back.
    app.ask_panels.clear();
    let (buf, _) = draw_frame(&mut app, 40, 10);
    let (text_x, text_y, _) = composer_text(&app);
    assert_eq!(buf[(text_x, text_y)].fg, palette.text);
    assert_eq!(buf[(text_x - 2, text_y)].fg, palette.accent);
}

/// A large paste leaves a chip *inline*: the text around it keeps its line,
/// the cursor keeps typing after it, and only the chip carries the accent.
#[test]
fn test_a_paste_chip_sits_inline_in_the_draft() {
    let mut app = app_with_draft("see ");
    app.input.insert_str("1\n2\n3\n4"); // >2 lines: a chip
    let (buf, composer) = draw_frame(&mut app, 60, 10);
    let (text_x, text_y, _) = composer_text(&app);
    let palette = crate::config::ThemePalette::default();

    let width = composer.width as usize;
    let content = "│ ❯ see [Pasted text #1 +3 lines]";
    assert_eq!(
        card_row(&buf, composer, composer.y + 1),
        format!("{content}{}│", " ".repeat(pad(width, content))),
        "the chip stays on the draft's own row:\n{}",
        frame_text(&buf)
    );
    assert_eq!(composer.height, 1 + 2, "and it adds no row of its own");

    // "see " is draft text, the chip is a chip (the accent is its only mark).
    assert_eq!(buf[(text_x, text_y)].fg, palette.text, "the typed text");
    let chip_x = text_x + "see ".chars().count() as u16;
    assert_eq!(buf[(chip_x, text_y)].fg, palette.accent, "the chip");
    assert_eq!(
        buf[(chip_x + 1, text_y)].fg,
        palette.accent,
        "the whole label, not its first cell"
    );
}

/// A chip that does not fit the row moves to the next one *whole*: the label
/// is never torn in half. (A chip wider than the row itself overflows and is
/// clipped — the label's start still says what it is.)
#[test]
fn test_a_paste_chip_never_breaks_in_half() {
    let mut app = app_with_draft("see ");
    app.input.insert_str("1\n2\n3\n4");
    // Width 24 → a 16-column text area: "see " + the 25-char chip cannot fit.
    let (buf, composer) = draw_frame(&mut app, 24, 11);
    assert_eq!(composer.height, 2 + 2, "the chip opens a row of its own");

    let first = card_row(&buf, composer, composer.y + 1);
    let second = card_row(&buf, composer, composer.y + 2);
    assert!(first.starts_with("│ ❯ see"), "{first}");
    assert!(
        second.starts_with("│   [Pasted text #1"),
        "the label starts on the second row: {second}"
    );
    assert!(
        !first.contains("[Pasted"),
        "nothing of the label is left behind: {first}"
    );
}

/// A compressed picture of `bytes` bytes, 4 × 2.
fn picture(bytes: usize) -> crate::util::image_compress::CompressedImage {
    crate::util::image_compress::CompressedImage {
        bytes: vec![0; bytes],
        width: 4,
        height: 2,
    }
}

/// An image chip draws like a paste chip: inline on the draft's own row, the
/// whole label in the accent colour, no row of its own.
#[test]
fn test_an_image_chip_sits_inline_in_the_draft() {
    let mut app = app_with_draft("see ");
    app.input.insert_image(picture(4096)); // → "[Image #1 4×2 4KB]"
    let (buf, composer) = draw_frame(&mut app, 60, 10);
    let (text_x, text_y, _) = composer_text(&app);
    let palette = crate::config::ThemePalette::default();

    let width = composer.width as usize;
    let content = "│ ❯ see [Image #1 4×2 4KB]";
    assert_eq!(
        card_row(&buf, composer, composer.y + 1),
        format!("{content}{}│", " ".repeat(pad(width, content))),
        "the chip stays on the draft's own row:\n{}",
        frame_text(&buf)
    );
    assert_eq!(composer.height, 1 + 2, "and it adds no row of its own");

    // "see " is draft text, the label is a chip (the accent is its only mark).
    assert_eq!(buf[(text_x, text_y)].fg, palette.text, "the typed text");
    let chip_x = text_x + "see ".chars().count() as u16;
    assert_eq!(buf[(chip_x, text_y)].fg, palette.accent, "the chip");
    assert_eq!(
        buf[(chip_x + 1, text_y)].fg,
        palette.accent,
        "the whole label, not its first cell"
    );
}

/// An image chip never breaks in half either — the wrapper knows one kind of
/// chip, not two.
#[test]
fn test_an_image_chip_never_breaks_in_half() {
    let mut app = app_with_draft("see ");
    app.input.insert_image(picture(4096));
    // Width 24 → a 16-column text area: "see " + the 19-char label cannot fit.
    let (buf, composer) = draw_frame(&mut app, 24, 11);
    assert_eq!(composer.height, 2 + 2, "the chip opens a row of its own");

    let first = card_row(&buf, composer, composer.y + 1);
    let second = card_row(&buf, composer, composer.y + 2);
    assert!(first.starts_with("│ ❯ see"), "{first}");
    assert!(
        second.starts_with("│   [Image #1"),
        "the label starts on the second row: {second}"
    );
    assert!(
        !first.contains("[Image"),
        "nothing of the label is left behind: {first}"
    );
}

/// A squeezed composer (the terminal is too short for the whole layout) gets
/// fewer rows than the height request asked for, so the card degrades to a
/// bare text area — and the editor must wrap at *those* columns. The chrome
/// the editor reads and the one the widget drew with are the same object, so
/// a click or an arrow key can never address a row the screen does not have.
#[test]
fn test_a_squeezed_card_wraps_at_the_columns_it_was_drawn_with() {
    let mut app = app_with_draft("a draft long enough to wrap somewhere");
    let (_, composer) = draw_frame(&mut app, 20, 6);
    let chrome = app.input.chrome();

    assert!(
        !chrome.card,
        "no room for a frame at {composer:?}: the composer degrades"
    );
    assert_eq!(
        (chrome.text_width, chrome.text_rows),
        (composer.width, composer.height),
        "the editor wraps at exactly what was drawn"
    );
}

#[test]
fn test_the_rails_are_not_text() {
    let mut app = app_with_draft("hello world");
    let (buf, composer) = draw_frame(&mut app, 40, 10);
    let (text_x, text_y, _) = composer_text(&app);

    // The pointer's claim covers the whole block (the app dispatches to the
    // composer), but the mapping — which is what a press turns into — finds no
    // text on the frame.
    assert!(app.composer_contains(composer.x + 4, composer.y));
    assert!(app.composer_contains(composer.x + 4, composer.bottom() - 1));
    assert_eq!(
        app.handle_mouse(press((composer.x + 4, composer.y))),
        MouseOutcome::Ignored,
        "a press on the top rail changes nothing:\n{}",
        frame_text(&buf)
    );
    assert!(!app.selection.is_press_active());

    // The draft's first cell is where the card's frame says it is.
    assert_eq!(text_y, composer.y + 1);
    assert_eq!(text_x, composer.x + 4);
    assert_eq!(buf[(text_x, text_y)].symbol(), "h");
}
