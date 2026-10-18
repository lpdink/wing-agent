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

/// 名牌的形态：宽终端上是一条分节线（不是框），80 列终端同样整行不折。
#[test]
fn wide_terminal_draws_the_letterhead_rule() {
    // 断言只看 100% 由欢迎屏决定的 header 行：整帧里滚动条的空闲 thumb 也画
    // `┃`（`app/tests/scrollbar.rs` 的 `is_bar_glyph`），拿整帧数字形会被它带偏。
    let mut app = test_app();
    // 100 列：内容 98，信息列 58 列 → 2x 大字 + 分节线。
    frame_body(&mut app, 100, 30);
    let header: String = app
        .chat
        .header_lines()
        .iter()
        .map(|line| line.to_string())
        .collect();
    assert!(header.contains('─'), "分节线得在：\n{header}");
    for gone in ['┏', '┓', '┃', '┗', '┛'] {
        assert!(!header.contains(gone), "框退役了（{gone}）：\n{header}");
    }

    let mut app = test_app();
    // 80 列：内容 78，信息列 38 列 → 1x 大字，键位行整行放得下。
    let body = frame_body(&mut app, 80, 30);
    assert!(compact(&body).contains("Esc中断"), "键位行还在：\n{body}");
    assert!(
        compact(&body).contains("Ctrl+J换行"),
        "键位行整行不折：\n{body}"
    );
}

/// 空会话是一屏开屏：整块在视口里居中（上下留白 + 左右留白）。
#[test]
fn an_empty_session_centers_the_block() {
    let mut app = test_app();
    let body = frame_body(&mut app, 100, 30);
    let rows: Vec<&str> = body.lines().collect();
    let art_top = rows
        .iter()
        .position(|row| row.contains('▀') || row.contains('▄'))
        .expect("海鸥 / 大字在");
    // 状态栏 1 行 + 上方留白（30 行视口里 15 行的块按 45% 分）+ 首行。
    assert!(
        art_top >= 5,
        "空会话上方要有留白，实际第 {art_top} 行：\n{body}"
    );
    let left = rows[art_top].chars().take_while(|c| *c == ' ').count();
    assert!(left > 0, "整块水平居中（左留白 {left}）：\n{body}");

    // 有了消息就退回顶对齐 + 左对齐（header 变成会话的开头）。
    let mut app = test_app();
    app.chat
        .push(crate::ui::chat_view::ChatCell::UserMessage("hello".into()));
    let body = frame_body(&mut app, 100, 30);
    let rows: Vec<&str> = body.lines().collect();
    let art_top = rows
        .iter()
        .position(|row| row.contains('▀') || row.contains('▄'))
        .expect("海鸥 / 大字在");
    // 状态栏 1 行 + 一格留白 + 站姿头顶的透明像素行 = 3。
    assert_eq!(art_top, 3, "有消息时只有一格呼吸：\n{body}");
}

/// 开屏是一块：牌子在上、输入卡紧贴在它下面（同宽、居中），空气都在下面。
#[test]
fn the_splash_puts_the_composer_under_the_nameplate() {
    let mut app = test_app();
    let mut terminal = test_terminal(100, 30);
    draw(&mut app, &mut terminal);

    let band = app.chat.geometry().area;
    let composer = app.geometry.composer_rect();
    assert_eq!(
        composer.y,
        band.bottom() + 1,
        "输入卡紧贴在牌子下面（中间一格呼吸）"
    );
    assert!(
        composer.bottom() + 3 <= 30,
        "下面的空气留给弹层：{composer:?}"
    );
    // 同宽同列：输入卡与牌子是一条边（牌子在 band 里水平居中）。
    let block = app.welcome.as_ref().expect("welcome").block_width();
    assert_eq!(composer.width, block, "输入卡与牌子同宽");
    assert_eq!(
        composer.x,
        band.x + (band.width - block) / 2,
        "两者居中在同一块里"
    );
}

/// 敲 `/` 不会让开屏整块挪位（S1）：弹层在开屏里是浮层，挂在输入卡下面。
#[test]
fn a_popup_does_not_move_the_splash() {
    let hero_top = |app: &mut App| -> u16 {
        let mut terminal = test_terminal(100, 30);
        draw(app, &mut terminal);
        app.chat.geometry().area.y
    };
    let mut app = test_app();
    let before = hero_top(&mut app);

    app.input.set_text("/");
    app.update_popup_from_input();
    let mut terminal = test_terminal(100, 30);
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.chat.geometry().area.y,
        before,
        "打开命令列表不该推动牌子"
    );
    // 弹层挂在输入卡下面（浮层），不占排版：输入卡下面的空气里有东西。
    let composer = app.geometry.composer_rect();
    let below: Vec<String> = (composer.bottom()..30)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                .collect()
        })
        .collect();
    let first_popup = below
        .iter()
        .find(|row| !row.trim().is_empty())
        .unwrap_or_else(|| panic!("弹层没画在输入卡下面：{below:?}"));
    // 与卡片**同列**：`splash_popup` 的 rect 已经是卡片矩形，不该再缩进一层
    // （缩了就会比卡片窄 2 列、相对卡片多缩进 1 列）。
    let lead = first_popup.chars().take_while(|c| *c == ' ').count() as u16;
    assert!(
        lead >= composer.x && lead < composer.right(),
        "弹层要落在卡片的横向范围内（lead={lead} card={composer:?}）：{first_popup:?}"
    );
}

/// 第一条消息落地：开屏退位 —— 牌子回到 transcript 顶部（一格留白），
/// 输入卡回到屏幕底部。
#[test]
fn the_first_message_drops_the_composer_back_to_the_bottom() {
    let mut app = test_app();
    let mut terminal = test_terminal(100, 30);
    draw(&mut app, &mut terminal);
    assert!(app.geometry.composer_rect().y > 10, "开屏时输入卡在中间");

    app.chat
        .push(crate::ui::chat_view::ChatCell::UserMessage("hello".into()));
    let mut terminal = test_terminal(100, 30);
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.geometry.composer_rect().bottom(),
        30,
        "有了消息，输入卡钉回底部"
    );
    assert_eq!(
        app.chat.geometry().area.y,
        1,
        "牌子回到会话开头（状态栏下面就是一格留白）"
    );
}

/// 回归：开屏的**空气也要算进"装不装得下"**。
///
/// 少算它的时候，草稿一长布局约束总和就超出屏高 —— ratatui 挤的是**牌子**
/// （`Length` 也能被压）：band 比 header 还矮、名牌最后一行被整行裁掉，而且
/// `content_height > band` 会顺手画出一条开屏上本不该有的滚动条。
#[test]
fn a_long_draft_never_squeezes_the_nameplate() {
    for (width, height, draft_lines) in [(100u16, 30u16, 10usize), (60, 26, 8)] {
        let mut app = test_app();
        app.input.set_text(&"draft line\n".repeat(draft_lines));
        let mut terminal = test_terminal(width, height);
        draw(&mut app, &mut terminal);

        let band = app.chat.geometry().area;
        let header = app.chat.header_height();
        assert!(
            band.height as usize >= header,
            "{width}x{height} + {draft_lines} 行草稿：band {} < header {header}",
            band.height
        );
        assert!(
            app.chat.content_height() <= band.height as usize,
            "{width}x{height} + {draft_lines} 行草稿：内容比 band 高（会画出滚动条）"
        );
    }
}

/// 回归：**一帧最多建一次 header**。
///
/// 开屏判定要先按开屏宽度量一次牌子；矮终端上装不下、每帧都退回会话版式 ——
/// 结论不缓存的话，每帧都会"先按开屏建一遍、再按会话重建一遍"（扫光期 ~25fps、
/// 每次击键各两遍）。缓存之后稳态一帧零重建（版式没变，`built_view` 命中）。
#[test]
fn a_settled_frame_builds_the_header_at_most_once() {
    // 矮终端：开屏装不下，退回会话版式（这条路径过去每帧建两遍）。
    let mut app = test_app();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);

    // 扫光期本来就每帧重建一次；这里钉的是"**一次**，不是两次"。
    for frame in 0..3 {
        let mut next = test_terminal(60, 12);
        let before = app.welcome_builds();
        draw(&mut app, &mut next);
        assert!(
            app.welcome_builds() - before <= 1,
            "第 {frame} 帧建了 {} 次（一帧最多一次）",
            app.welcome_builds() - before
        );
    }
}

/// 矮终端装不下开屏版式：退回会话版式（牌子贴顶、输入卡钉底），不硬挤。
#[test]
fn a_short_terminal_falls_back_to_the_conversation_layout() {
    let mut app = test_app();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.geometry.composer_rect().bottom(),
        12,
        "矮屏上输入卡钉底"
    );
    assert_eq!(app.chat.geometry().area.y, 1, "牌子贴顶，不做光学居中");
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

    // 最窄档：连文字列也放不下，只剩一行 `wing` + 版本。开屏的 band 是终端整宽
    // （没有滚动条 gutter —— 空会话没有可滚的内容），所以这一档从 38 列起。
    let mut app = test_app();
    let body = frame_body(&mut app, 38, 30);
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

        // 开屏（空会话）的 band 就是终端整宽：没有滚动条，也就没有 gutter。
        // 有消息之后才是 `content_area`（终端宽 − 2）。
        let content_width = if app.chat.is_empty() {
            app.geometry.chat_band().width as usize
        } else {
            let area = crate::ui::scrollbar::content_area(app.geometry.chat_band()).width as usize;
            assert!(
                area < width as usize,
                "内容区确实比终端窄（gutter 是常量 2）"
            );
            area
        };
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

    app.sync_welcome(&palette, crate::ui::welcome::Viewport::width_only(100), now);
    let first = header_text(&app);
    app.sync_welcome(
        &palette,
        crate::ui::welcome::Viewport::width_only(100),
        now + Duration::from_millis(400),
    );
    assert_ne!(first, header_text(&app), "扫光期间 header 要跟着进度重建");

    let settled = now + Duration::from_millis(crate::ui::welcome::SWEEP_MS + 1);
    app.sync_welcome(
        &palette,
        crate::ui::welcome::Viewport::width_only(100),
        settled,
    );
    let frozen = header_text(&app);

    // 扫光定格后：同一时刻再 sync 不重建。
    app.sync_welcome(
        &palette,
        crate::ui::welcome::Viewport::width_only(100),
        settled,
    );
    assert_eq!(frozen, header_text(&app), "定格后同一时刻不重建");

    // 海鸥是常驻 idle 循环，但重绘只发生在规划器的 deadline 上，不是每帧：
    // 定格后连着一毫秒一毫秒地 sync，header 必须纹丝不动。
    for step in 1..6u64 {
        app.sync_welcome(
            &palette,
            crate::ui::welcome::Viewport::width_only(100),
            settled + Duration::from_millis(step),
        );
        assert_eq!(
            frozen,
            header_text(&app),
            "定格后逐毫秒 sync 不该重建（step={step}）"
        );
    }
    // 而跨过几个 deadline 之后 header 必须变 —— idle 动画真的在走。
    let alive = settled + Duration::from_secs(6);
    app.sync_welcome(
        &palette,
        crate::ui::welcome::Viewport::width_only(100),
        alive,
    );
    assert_ne!(frozen, header_text(&app), "6s 后 idle 动画应当已经动过");

    // 40 列会掉出整块布局（海鸥被撤），header 一定长得不一样；用 70 这种
    // "同档、文字又刚好不省略"的宽度是测不出重建的。
    app.sync_welcome(
        &palette,
        crate::ui::welcome::Viewport::width_only(40),
        alive,
    );
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
            crate::ui::welcome::Viewport::width_only(100 + step as u16),
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
    app.sync_welcome(
        &palette,
        crate::ui::welcome::Viewport::width_only(100),
        settled,
    );

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
    app.sync_welcome(
        &palette,
        crate::ui::welcome::Viewport::width_only(100),
        settled + Duration::from_secs(5),
    );
    assert_eq!(hidden, header_text(&app), "滚出视口后不该重建");

    // 滚回顶部：立刻恢复（可见性翻转强制重建一次）。
    app.chat.scroll_offset = 0;
    assert!(app.chat.header_in_view());
    app.sync_welcome(
        &palette,
        crate::ui::welcome::Viewport::width_only(100),
        settled + Duration::from_secs(5),
    );
    assert_ne!(hidden, header_text(&app), "回到视口要立刻重绘");
}

/// 名牌右列的**事实槽位**：版本行的下一行（`2 skills · 1 rule`，缺席时空白）。
///
/// 框退役之后没有边线可切了：那一行前面是海鸥的字形（半格 `▀`/`▄`）与间隔，
/// 版本行之后的部分才是名牌。按"版本行里 `wing · ` 的列"定位，取同一列起的
/// 尾部文本。
///
/// 帧级断言刻意不写成"整帧不含某个词"：tip 是按**时间种子**抽的
/// （`tips::seed_now()`），池子里有 `/skills 看已装技能…` —— 任何对整帧做
/// 子串扫描的断言都会随进程抖动。槽位是确定性的：它只由事实本身决定。
fn facts_slot(app: &App) -> String {
    let lines = app.chat.header_lines();
    let version = lines
        .iter()
        .position(|line| line.spans.iter().any(|s| s.content.contains("wing · ")))
        .expect("版本行在名牌右列");
    let version_text: String = lines[version]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    // `find` 是字节偏移（半格字形 `▀` 是 3 字节），下面按字符跳。
    let byte = version_text.find("wing · ").expect("版本行带 brand");
    let column = version_text[..byte].chars().count();
    let line: String = lines[version + 1]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    if line.chars().count() <= column {
        return String::new();
    }
    line.chars()
        .skip(column)
        .collect::<String>()
        .trim()
        .to_string()
}

/// 开屏 = 名牌 + 输入卡：会话事实（skills / rules）挂在名牌右列，
/// transcript 里既没有 system 消息，也没有别的 cell。
#[test]
fn the_start_screen_shows_session_facts_in_the_nameplate() {
    let mut app = test_app();
    app.handle_event(crate::protocol::WingEvent::SyncSession {
        session_id: "test-session".into(),
        messages: vec![],
        uncommitted: None,
        uncommitted_tools: vec![],
        events: vec![],
        status: crate::protocol::SessionStatus::Idle,
        turn_started_at: None,
        agent: Some(Box::new(crate::protocol::AgentInfo {
            model_name: "test-model".into(),
            model_id: None,
            system_prompt: None,
            tools: vec![],
            skills: vec!["pdf".into(), "webapp".into()],
            rules: vec!["AGENTS.md".into()],
            workspace: None,
            provider_name: None,
            model_display_name: None,
        })),
        name: None,
        draft: None,
        usage: None,
        meta: crate::protocol::EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: Some("test-session".into()),
            request_id: "r".into(),
        },
    });

    let body = frame_body(&mut app, 100, 30);
    assert_eq!(
        facts_slot(&app),
        "2 skills · 1 rule",
        "事实行在名牌右列：\n{body}"
    );
    assert!(
        compact(&body).contains("2skills·1rule"),
        "事实行真的画在帧上（不是只存在于 header 行里）：\n{body}"
    );
    // 事实不是 chat cell：transcript 里什么都没有（旧版在顶部推一条
    // `loaded 2 skills, 1 rules · /skills for details`）。
    assert!(app.chat.cells.is_empty(), "transcript 里不该有任何 cell");

    // 老网关（没有 agent 快照）：槽位留空，名牌高度不变。
    let mut app = test_app();
    app.handle_event(sync_event(vec![], None, vec![], vec![], None));
    let body = frame_body(&mut app, 100, 30);
    assert_eq!(facts_slot(&app), "", "没有会话事实时槽位是空白：\n{body}");
    assert!(app.chat.cells.is_empty());
}
