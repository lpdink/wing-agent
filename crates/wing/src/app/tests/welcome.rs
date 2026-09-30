//! Welcome block lane tests — the header the app builds, keeps in sync and
//! takes down for tests.
//!
//! The block's own rendering (art, gradient, ladder, elision) is pinned in
//! `ui::welcome`; what is asserted here is the wiring: the app owns a welcome,
//! rebuilds the header while the sweep runs, and stops once it settles.

use std::time::Duration;

use super::support::*;
use crate::app::App;

/// The frame's text, all rows joined.
fn frame_body(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = test_terminal(width, height);
    draw(app, &mut terminal);
    frame_text(terminal.backend().buffer())
}

/// Whitespace-stripped copy of a frame.
///
/// `frame_text` reads the buffer cell by cell, and a double-width glyph leaves
/// its second cell empty — a Chinese line therefore comes back with a space
/// inside every character pair. Comparisons strip whitespace so they read the
/// characters, not the cell grid.
fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The header lines serialized **including styles** — the sweep moves colors,
/// not text, so a content-only fingerprint would miss the rebuild it is here
/// to prove.
fn header_text(app: &App) -> String {
    app.chat
        .header_lines()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| format!("{:?}{:?}{}", span.style.fg, span.style.bg, span.content))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn welcome_header_shows_brand_version_and_tip() {
    let mut app = test_app();
    let body = frame_body(&mut app, 100, 30);

    assert!(body.contains("✦ wing"), "wordmark 在首帧里：\n{body}");
    assert!(
        body.contains("dev"),
        "开发构建显示 dev 而不是 v0.0.0：\n{body}"
    );
    assert!(
        body.contains(&crate::shared::constants::TIPS_COMMAND),
        "入口提示在：\n{body}"
    );
    let tip = app.tip_text();
    assert!(
        compact(&body).contains(&compact(tip)),
        "首帧应当带上抽中的 tip「{tip}」：\n{body}"
    );
}

#[test]
fn welcome_header_never_carries_the_retired_release_notes() {
    let mut app = test_app();
    let body = frame_body(&mut app, 100, 30);
    // 旧 header 的硬编码 "What's new" 盒子不许回来：那份文案从闭源时代起就没
    // 更新过，正是它把开屏变成了一句永远不成立的话。
    for gone in ["What's new", "Async proactive context compaction"] {
        assert!(!body.contains(gone), "旧 release notes 不该再出现：{gone}");
    }
}

#[test]
fn narrow_terminal_drops_the_art_before_the_text() {
    let mut app = test_app();
    let body = frame_body(&mut app, 40, 30);
    assert!(body.contains("✦ wing"), "窄屏也要有 wordmark：\n{body}");
    assert!(
        !body.contains('█') && !body.contains('▀') && !body.contains('▄'),
        "窄屏先撤标记：\n{body}"
    );
    // 任何宽度都不许把行撑破（ChatView 的 Paragraph 不换行，超了就是被切）。
    for width in [24u16, 60, 79, 80, 120] {
        let mut app = test_app();
        let body = frame_body(&mut app, width, 30);
        for row in body.lines() {
            let bar = row.split('|').next_back().unwrap_or("");
            assert!(
                unicode_width::UnicodeWidthStr::width(bar) <= width as usize,
                "宽 {width} 时行超宽：{row}"
            );
        }
    }
}

#[test]
fn header_is_rebuilt_while_sweeping_and_frozen_after() {
    let mut app = test_app();
    let palette = app.palette();
    let now = std::time::Instant::now();

    app.sync_welcome(&palette, 100, now);
    let first = header_text(&app);
    app.sync_welcome(&palette, 100, now + Duration::from_millis(400));
    assert_ne!(first, header_text(&app), "扫光期间 header 要跟着进度重建");

    let settled = now + Duration::from_millis(crate::ui::welcome::SWEEP_MS + 1);
    app.sync_welcome(&palette, 100, settled);
    let frozen = header_text(&app);
    app.sync_welcome(&palette, 100, settled + Duration::from_secs(30));
    assert_eq!(frozen, header_text(&app), "定格后不再重建");

    app.sync_welcome(&palette, 70, settled + Duration::from_secs(31));
    assert_ne!(frozen, header_text(&app), "缩放要按新宽度重建");
}

#[test]
fn clear_welcome_leaves_a_bare_top() {
    let mut app = test_app();
    app.clear_welcome();
    app.chat.push(crate::ui::chat_view::ChatCell::UserMessage(
        "hello world".into(),
    ));
    let body = frame_body(&mut app, 100, 30);
    assert!(!body.contains("✦ wing"), "关掉之后不该再画：\n{body}");
    // 下一次 draw 也不会把它装回来（sync_welcome 拿的是 None）。
    let body = frame_body(&mut app, 100, 30);
    assert!(!body.contains("✦ wing"), "draw 不该复活 header：\n{body}");
}

#[test]
fn the_tip_does_not_rotate_between_frames() {
    let mut app = test_app();
    let tip = app.tip_text();
    let palette = app.palette();
    let now = std::time::Instant::now();
    for step in 1..8u64 {
        app.sync_welcome(
            &palette,
            100 + step as u16,
            now + Duration::from_millis(step * 40),
        );
        assert_eq!(app.tip_text(), tip, "重绘不该换 tip");
    }
}

#[test]
fn slash_tips_lists_the_whole_pool() {
    let mut app = test_app();
    assert!(app.try_frontend_command("/tips"));
    let cell = app
        .chat
        .cells
        .iter()
        .find_map(|c| match c.cell() {
            crate::ui::chat_view::ChatCell::SystemMessage(text) => Some(text.clone()),
            _ => None,
        })
        .expect("/tips 应当落成一个系统消息 cell");
    for tip in crate::shared::tips::TIPS {
        assert!(cell.contains(tip.text), "缺 tip：{}", tip.text);
    }
}
