//! 整屏指纹 —— 开屏整块（状态栏 + 牌子 + 输入卡）在一帧里的几何。
//!
//! 断言的是**位置**，不是像素：牌子停在视口哪里、分节线多长、输入卡边框落在哪一行
//! 哪一列、状态栏两端画到没有、有没有一行溢出。任何一条变红都意味着开屏的版式动过
//! 了 —— 那正是要被人看一次的东西（改常量就顺手改这里）。
//!
//! 帧走 [`crate::app::preview`]：它与 `cargo run --example splash_preview` 是同一份
//! 取样（真 App → 真 `App::draw` → 真 `Buffer`）。所以这里钉的既是真机，也是那个
//! 创作期工具给你看的那一张 —— 工具漂了，断言跟着红。

use crate::app::preview;
use crate::app::preview::Spec;
use crate::config::AppConfig;
use crate::config::Appearance;
use crate::config::ColorPreset;
use crate::ui::shimmer::to_rgb;
use crate::ui::welcome::sprite;
use ratatui::style::Color;

/// 一帧（固定配置：不读 `$WING_HOME`，明暗由 `preset` 定）。
fn frame(preset: ColorPreset, width: u16, height: u16) -> preview::Frame {
    preview::sample_with(
        AppConfig::default(),
        &Spec {
            width,
            height,
            preset: Some(preset),
            ..Spec::default()
        },
    )
}

/// 一行里从第 `column` 列起的文本。
///
/// `rows()` 是**格子**投影（一个格子一个字符，字符串下标 = 屏幕列号），所以这里按
/// 列跳；CJK 的续格是单独一个空格格。
fn tail(row: &str, column: usize) -> String {
    row.chars().skip(column).collect()
}

/// 去掉空白后的文本（CJK 的续格在格子里是空格，比对文案时先抹掉）。
fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 状态栏之后第一行有墨迹的行号 —— 开屏的空气量，`splash_pad` 一变它就变。
fn first_ink(rows: &[String]) -> usize {
    1 + rows[1..]
        .iter()
        .position(|row| !row.trim().is_empty())
        .expect("牌子总在")
}

/// 分节线：(行号, 长度)。名牌右列从 `column` 起整段都是 `─` 的那一行 —— 同一行
/// 左边是海鸥的墨迹，所以不能按整行判。
fn rule(rows: &[String], column: usize) -> (usize, usize) {
    rows.iter()
        .enumerate()
        .find_map(|(y, row)| {
            let tail = tail(row, column);
            (!tail.is_empty() && tail.chars().all(|c| c == '─')).then(|| (y, tail.chars().count()))
        })
        .expect("分节线得在")
}

/// 输入卡的上下边框：(顶行, 底行, 左列, 右列) —— 卡是圆角框，四个角各一格。
fn card(rows: &[String]) -> (usize, usize, usize, usize) {
    let top = rows
        .iter()
        .position(|row| row.contains('╭'))
        .expect("输入卡的上边框");
    let bottom = rows
        .iter()
        .position(|row| row.contains('╰'))
        .expect("输入卡的下边框");
    let left = rows[top].chars().position(|c| c == '╭').expect("左上角");
    let right = rows[top].chars().position(|c| c == '╮').expect("右上角");
    assert_eq!(
        rows[bottom].chars().position(|c| c == '╰'),
        Some(left),
        "上下边框同列"
    );
    assert_eq!(
        rows[bottom].chars().position(|c| c == '╯'),
        Some(right),
        "上下边框同宽"
    );
    (top, bottom, left, right)
}

const SESSION_ID: &str = "20261005-213012-ab12cd34";

/// 版本行里能放下的 commit 前缀（长 SHA 构建时整串放不进块宽，断言只看前 7 位）。
fn short_commit() -> String {
    env!("WING_COMMIT_HASH").chars().take(7).collect()
}

#[test]
fn the_splash_block_sits_at_the_optical_centre_at_120x40() {
    let rows = frame(ColorPreset::Dark, 120, 40).rows();
    assert_eq!(rows.len(), 40, "帧高 = 终端高");

    // 状态栏：整行画满，左端 brand、右端连接状态 —— 两端都在，中间没被挤掉。
    assert_eq!(
        rows[0].chars().count(),
        120,
        "状态栏画满整行：{:?}",
        rows[0]
    );
    assert!(rows[0].starts_with(" Wing · "), "左端：{:?}", rows[0]);
    assert!(rows[0].contains(SESSION_ID), "宽终端画完整会话 id");
    assert!(rows[0].ends_with('●'), "右端：{:?}", rows[0]);
    assert!(
        rows[0].contains("45.2k / 200k"),
        "上下文水位在：{:?}",
        rows[0]
    );

    // 牌子整块：状态栏 1 行 + 上方空气 10 行，海鸥盒的第一格墨迹落在第 11 行
    // （盒顶那一行是透明像素行）。这个数字是 `splash_pad` 的 45% 光学中心。
    assert_eq!(first_ink(&rows), 11, "开屏的空气量变了");

    // 名牌右列：版本 / 事实 / 分节线 / 键位 / tip / 入口，全部从第 57 列起
    // （居中位移 17 + 海鸥盒 40）。块宽 86 = 40 + 46（2x 大字）。
    let version = tail(&rows[16], 57);
    assert!(version.starts_with("wing · "), "版本行：{version:?}");
    assert!(version.contains(&short_commit()), "带 commit：{version:?}");
    assert_eq!(tail(&rows[17], 57).trim_end(), "2 skills · 1 rule");
    assert_eq!(
        rule(&rows, 57),
        (18, 46),
        "分节线：块宽 46 列、从第 57 列起"
    );
    assert!(
        compact(&rows[19]).contains("Esc中断·Ctrl+J换行·Ctrl+C×2退出"),
        "键位行整行不折：{:?}",
        rows[19]
    );

    // 输入卡：与牌子同宽（86）同列（17..=102），紧贴牌子下方。中间两行空白各有出处：
    // 第 23 行是牌子自己的收尾空行，第 24 行是「牌子与输入卡之间一格呼吸」。
    let (top, bottom, left, right) = card(&rows);
    assert_eq!(
        (top, bottom, left, right),
        (25, 27, 17, 102),
        "输入卡的位置与宽度"
    );
    assert!(
        rows[23].is_empty() && rows[24].is_empty(),
        "牌子收尾 + 一格呼吸"
    );
    assert_eq!(right - left + 1, 86, "输入卡与牌子同宽（整块 86 列）");
    // 下面的空气留给弹层与草稿长大：至少三行。
    assert!(
        40 - 1 - card(&rows).1 >= 3,
        "下方空气：{:?}",
        rows[27..].len()
    );
}

#[test]
fn the_splash_block_sits_at_the_optical_centre_at_80x24() {
    // 80 列是多数终端的默认宽：名牌掉到 1x 大字，块宽 78（40 + 38）。
    let rows = frame(ColorPreset::Dark, 80, 24).rows();
    assert_eq!(rows.len(), 24);

    assert_eq!(rows[0].chars().count(), 80);
    assert!(rows[0].starts_with(" Wing · "));
    assert!(
        rows[0].contains("…ab12cd34"),
        "窄终端退化尾 8 位：{:?}",
        rows[0]
    );
    assert!(!rows[0].contains(SESSION_ID), "完整 id 放不下");
    assert!(rows[0].ends_with('●'));

    // 空气 (24 - 1 - 14 - 4) * 45% = 2 行，盒顶一行透明 → 首格墨迹在第 4 行。
    assert_eq!(first_ink(&rows), 4, "开屏的空气量变了");

    let version = tail(&rows[8], 41);
    assert!(version.starts_with("wing · "), "版本行：{version:?}");
    assert_eq!(tail(&rows[9], 41).trim_end(), "2 skills · 1 rule");
    assert_eq!(rule(&rows, 41), (10, 38), "1x 档的块宽 = 38 列");
    assert!(
        compact(&rows[11]).contains("Esc中断·Ctrl+J换行·Ctrl+C×2退出"),
        "80 列键位行整行放得下：{:?}",
        rows[11]
    );

    assert_eq!(card(&rows), (18, 20, 1, 78), "输入卡与牌子同宽（78）同列");
    assert!(rows[17].is_empty(), "牌子与输入卡之间一格呼吸");
    assert_eq!(24 - 1 - card(&rows).1, 3, "下方三行空气");
}

#[test]
fn the_narrow_ladder_keeps_the_block_centred() {
    // 40 列：掉到 Compact（撤海鸥、1x 大字 + 名牌），块宽 38、居中位移 1。
    let rows = frame(ColorPreset::Dark, 40, 20).rows();
    assert_eq!(rows.len(), 20);
    assert_eq!(first_ink(&rows), 3, "海鸥撤了，1x 大字从第 3 行起");
    let version = tail(&rows[6], 1);
    assert!(version.starts_with("wing · "), "版本行：{version:?}");
    assert!(version.contains(&short_commit()), "带 commit：{version:?}");
    assert_eq!(rule(&rows, 1), (8, 38), "窄档块宽 38");
    assert!(compact(&rows[9]).contains("Esc中断·Ctrl+J换行·Ctrl+C×2退出"));
    assert_eq!(card(&rows), (14, 16, 1, 38), "输入卡跟着块宽收窄");
}

#[test]
fn the_two_appearances_share_the_geometry_and_change_the_ink() {
    for (width, height) in [(120u16, 40u16), (80, 24)] {
        let dark = frame(ColorPreset::Dark, width, height);
        let light = frame(ColorPreset::Light, width, height);
        assert_eq!(
            dark.rows(),
            light.rows(),
            "{width}x{height}：明暗只换颜色，不换排版"
        );
        assert_eq!(dark.palette().appearance, Appearance::Dark);
        assert_eq!(light.palette().appearance, Appearance::Light);
        assert_ne!(dark.ansi(), light.ansi(), "两套 palette 得真的画出来");

        // 海鸥的羽色是两套 palette 里最诚实的那条线索（白底上羽白等于透明，
        // 所以 light 换一档浅蓝灰）：帧里必须找得到各自那一档。
        for (name, frame, appearance) in [
            ("dark", &dark, Appearance::Dark),
            ("light", &light, Appearance::Light),
        ] {
            let accent = to_rgb(frame.palette().accent);
            let feather = sprite::brand('W', accent, appearance).expect("羽色");
            let feather = Color::Rgb(feather.0, feather.1, feather.2);
            assert!(
                frame.colors().contains(&feather),
                "{width}x{height} {name}：海鸥的羽色 {feather:?} 没画出来"
            );
        }
    }
}

#[test]
fn no_frame_row_exceeds_the_terminal() {
    // 整块版面的硬约束（`Paragraph` 不换行，多一列就是被切一列）：任何尺寸下
    // 状态栏 / 牌子 / 输入卡都不许溢出一格。牌子自己的单测只覆盖 header，这里
    // 覆盖拼起来之后的整屏（含卡）。
    for width in [40u16, 60, 80, 100, 120, 200] {
        for height in [20u16, 24, 30, 40] {
            let frame = frame(ColorPreset::Dark, width, height);
            for (y, row) in frame.rows().iter().enumerate() {
                assert!(
                    row.chars().count() <= width as usize,
                    "{width}x{height} 第 {y} 行溢出：{row:?}"
                );
            }
            for (y, line) in frame.plain().lines().enumerate() {
                let used = unicode_width::UnicodeWidthStr::width(line);
                assert!(
                    used <= width as usize,
                    "{width}x{height} 第 {y} 行文本溢出（{used} 列）：{line:?}"
                );
            }
        }
    }
}
