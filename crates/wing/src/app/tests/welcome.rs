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

    assert!(
        compact(&body).contains("wing·dev"),
        "文本形态的 brand + 版本在首帧里：\n{body}"
    );
    assert!(
        body.contains("dev"),
        "开发构建显示 dev 而不是 v0.0.0：\n{body}"
    );
    assert!(
        body.contains(crate::shared::constants::TIPS_COMMAND),
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
    // 60 列：内容宽 60 - gutter 2 = 58 -> Compact 档（撤海鸥、留文字列）。
    // 40 列会掉到 Minimal（只剩一行 wordmark），那是另一档，下面单独测。
    let body = frame_body(&mut app, 60, 30);
    let compacted = compact(&body);
    assert!(
        compacted.contains("wing") && compacted.contains("dev·"),
        "窄屏也要有 wordmark + 版本：\n{body}"
    );
    // 海鸥撤了：它的琥珀喙 / 脚是只有海鸥才用的颜色，拿它当海鸥的指纹。
    let amber = ratatui::style::Color::Rgb(245, 169, 60);
    let gull_present = app.chat.header_lines().iter().any(|line| {
        line.spans
            .iter()
            .any(|span| span.style.fg == Some(amber) || span.style.bg == Some(amber))
    });
    assert!(!gull_present, "窄屏先撤海鸥：\n{body}");

    // 最窄档：连文字列也放不下，只剩一行 `wing` + 版本。
    let mut app = test_app();
    let body = frame_body(&mut app, 40, 30);
    let compacted = compact(&body);
    assert!(
        compacted.contains("wing") && compacted.contains("dev·"),
        "最窄档也要有 brand + 版本：\n{body}"
    );
    assert!(
        !compacted.contains("Esc中断"),
        "最窄档放不下键位行，不该显示：\n{body}"
    );
}

#[test]
fn welcome_header_fits_the_chat_band() {
    // 核心不变量：header 与消息渲染在同一个矩形里 —— band 减掉右侧滚动条
    // gutter —— 而 `Paragraph` 不换行，多一列就是被切一列（省略号首当其冲）。
    // 断言必须打在**构建出来的行**上：从画完的帧里读，读到的已经是被裁过的。
    for width in [24u16, 40, 60, 79, 80, 120, 200] {
        let mut app = test_app();
        let mut terminal = test_terminal(width, 30);
        draw(&mut app, &mut terminal);

        let content_width =
            crate::ui::scrollbar::content_area(app.geometry.chat_band()).width as usize;
        assert!(
            content_width < width as usize,
            "内容区确实比终端窄（gutter 是常量 2）"
        );
        for line in app.chat.header_lines() {
            let used: usize = line
                .spans
                .iter()
                .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            assert!(
                used <= content_width,
                "终端宽 {width}（内容区 {content_width}）时 header 行有 {used} 列：{line:?}"
            );
        }
    }
}

#[test]
fn header_rebuilds_on_the_sweep_then_only_at_planner_cadence() {
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

    // 扫光定格后：同一时刻再 sync 不重建。
    app.sync_welcome(&palette, 100, settled);
    assert_eq!(frozen, header_text(&app), "定格后同一时刻不重建");

    // 海鸥是常驻 idle 循环，但重绘只发生在规划器的 deadline 上，不是每帧：
    // 定格后连着一毫秒一毫秒地 sync，header 必须纹丝不动。
    for step in 1..6u64 {
        app.sync_welcome(&palette, 100, settled + Duration::from_millis(step));
        assert_eq!(
            frozen,
            header_text(&app),
            "定格后逐毫秒 sync 不该重建（step={step}）"
        );
    }
    // 而跨过几个 deadline 之后 header 必须变 —— idle 动画真的在走。
    let alive = settled + Duration::from_secs(6);
    app.sync_welcome(&palette, 100, alive);
    assert_ne!(frozen, header_text(&app), "6s 后 idle 动画应当已经动过");

    // 40 列会掉出整块布局（海鸥被撤），header 一定长得不一样；用 70 这种
    // "同档、文字又刚好不省略"的宽度是测不出重建的。
    app.sync_welcome(&palette, 40, alive);
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
    assert!(!body.contains("wing ·"), "关掉之后不该再画：\n{body}");
    // 下一次 draw 也不会把它装回来（sync_welcome 拿的是 None）。
    let body = frame_body(&mut app, 100, 30);
    assert!(!body.contains("wing ·"), "draw 不该复活 header：\n{body}");
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

#[test]
fn scrolling_the_welcome_out_of_view_parks_the_clock() {
    let mut app = test_app();
    let palette = app.palette();
    let now = std::time::Instant::now();
    // 先跨过扫光，让 block 进入"定格 + idle 循环"状态。
    let settled = now + Duration::from_millis(crate::ui::welcome::SWEEP_MS + 1);
    app.sync_welcome(&palette, 100, settled);

    let header_len = app.chat.header_lines().len();
    assert!(header_len > 0, "欢迎屏在");
    assert!(
        app.welcome
            .as_ref()
            .expect("welcome 在手")
            .next_frame(settled, false, true)
            .is_some(),
        "可见时 idle 循环有下一个 deadline"
    );

    // 滚出视口：整条时钟停摆，也不再重建。
    app.chat.scroll_offset = header_len;
    assert!(!app.chat.header_in_view());
    assert!(
        app.welcome
            .as_ref()
            .expect("welcome 在手")
            .next_frame(settled, false, false)
            .is_none(),
        "滚出视口后不该再有 tick"
    );
    let hidden = header_text(&app);
    app.sync_welcome(&palette, 100, settled + Duration::from_secs(5));
    assert_eq!(hidden, header_text(&app), "滚出视口后不该重建");

    // 滚回顶部：立刻恢复（可见性翻转强制重建一次）。
    app.chat.scroll_offset = 0;
    assert!(app.chat.header_in_view());
    app.sync_welcome(&palette, 100, settled + Duration::from_secs(5));
    assert_ne!(hidden, header_text(&app), "回到视口要立刻重绘");
}
