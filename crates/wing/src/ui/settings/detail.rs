//! 详情栏（宽屏右侧）与窄屏的 doc 提示行。
//!
//! 详情栏是「配置的正确性知识住在声明里」这句话的兑现处：`title` / `doc` / `notes` /
//! `default` / `example` / enum 的每个取值是什么意思 / 约束（范围、pattern、项数），
//! 全部来自 [`wing_api_client::models::SettingNode`]。
//!
//! 目录节点不是 `Row` 的字段（07 的访问器清单里没有「行 → 节点」），因此这里显式收一份
//! [`SettingsCatalogs`]（10 传的就是构造面板用的同一份；见 08 design.md D11 / A3）。
//! 取不到节点是**正常路径**：合成行、union 元素行、目录与文档对不上都会走到这里 ——
//! 这时详情栏退化为「label + 路径 + 一句说明」，不 panic、不留空白。

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::shared::panels::settings::Row;
use wing_api_client::models::ApplyScope;
use wing_api_client::models::SettingNode;

use super::text::LineBuf;
use super::text::single_row;

/// 详情栏的行列表（高度预算内；超长内容按行截断）。
pub(crate) fn detail_lines(
    node: Option<&SettingNode>,
    row: &Row,
    palette: &ThemePalette,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let text = Style::default().fg(palette.text);
    let title = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let mut buf = LineBuf::new(width, height);

    let heading = node.map_or_else(|| row.label.clone(), |n| n.display_label().to_string());
    buf.text(&heading, title);
    let Some(node) = node else {
        // 合成行 / 无法定位的节点：把已知的都说清楚，然后停。
        buf.blank();
        if !row.path.is_empty() {
            buf.prose(&format!("路径 {}", row.path), dim);
        }
        buf.blank();
        buf.prose("（无声明元信息：这一行没有对应的目录节点）", dim);
        return buf.take();
    };

    if !node.doc.trim().is_empty() {
        buf.blank();
        buf.prose(&node.doc, text);
    }
    for note in &node.notes {
        if note.trim().is_empty() {
            continue;
        }
        buf.blank();
        buf.prose(note, text);
    }

    buf.blank();
    if node.has_default
        && let Some(default) = &node.default
    {
        buf.push(Line::from(vec![
            Span::styled("默认  ".to_string(), dim),
            Span::styled(json_text(default), text),
        ]));
    }
    buf.push(Line::from(vec![
        Span::styled("生效  ".to_string(), dim),
        Span::styled(apply_label(&node.apply), text),
    ]));
    if node.secret {
        buf.push(Line::from(vec![
            Span::styled("密文  ".to_string(), dim),
            Span::styled("只写不回显（只显示末 4 位）".to_string(), dim),
        ]));
    }
    if !row.path.is_empty() {
        buf.prose(&format!("路径  {}", row.path), dim);
    }
    if let Some(example) = node.example.as_deref().filter(|e| !e.trim().is_empty()) {
        buf.prose(&format!("示例  {example}"), dim);
    }
    if !node.choices.is_empty() {
        buf.blank();
        buf.text("可选值", dim);
        let pad = node
            .choices
            .iter()
            .map(|choice| UnicodeWidthStr::width(choice.value.as_str()))
            .max()
            .unwrap_or(0)
            .min(16);
        for choice in &node.choices {
            let value_width = UnicodeWidthStr::width(choice.value.as_str());
            let gap = " ".repeat(pad.saturating_sub(value_width) + 2);
            let doc = choice.doc.clone().unwrap_or_default();
            buf.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(choice.value.clone(), text),
                Span::raw(gap),
                Span::styled(doc, dim),
            ]));
        }
    }
    if let Some(constraints) = constraints_text(node) {
        buf.blank();
        buf.prose(&format!("约束  {constraints}"), dim);
    }
    if !node.editable {
        buf.blank();
        buf.prose("这一项不可编辑（派生值 / 环境变量决定）", dim);
    }
    buf.take()
}

/// 窄屏：当前行的 `doc` 首行（没有 doc 时回落路径，再回落 `(无说明)`）。
pub(crate) fn doc_hint_line(
    node: Option<&SettingNode>,
    row: &Row,
    palette: &ThemePalette,
    width: usize,
) -> Line<'static> {
    if width == 0 {
        return Line::from("");
    }
    let dim = Style::default().fg(palette.dim);
    let text = node
        .map(|node| node.doc.trim())
        .filter(|doc| !doc.is_empty())
        .map(str::to_string)
        .or_else(|| (!row.path.is_empty()).then(|| row.path.clone()))
        .unwrap_or_else(|| "(无说明)".to_string());
    let first = text.lines().next().unwrap_or("").trim();
    Line::from(Span::styled(single_row(first, width), dim))
}

/// 生效域的中文措辞（详情栏与回执同源）。
pub(crate) fn apply_label(apply: &ApplyScope) -> String {
    match apply {
        ApplyScope::Hot => "热重载".to_string(),
        ApplyScope::NextSession => "新会话".to_string(),
        ApplyScope::Restart => "需重启网关".to_string(),
        ApplyScope::Readonly => "只读".to_string(),
        ApplyScope::Unknown(raw) => raw.clone(),
    }
}

/// 约束的摘要（范围 / 长度 / 正则 / 项数），没有约束时 `None`。
pub(crate) fn constraints_text(node: &SettingNode) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(min) = node.min {
        parts.push(format!("值 {}", range_op(min, node.exclusive_min, false)));
    }
    if let Some(max) = node.max {
        parts.push(format!("值 {}", range_op(max, node.exclusive_max, true)));
    }
    if let Some(len) = node.min_length {
        parts.push(format!("长度 ≥ {len}"));
    }
    if let Some(pattern) = node.pattern.as_deref().filter(|p| !p.trim().is_empty()) {
        parts.push(format!("格式 {pattern}"));
    }
    if let Some(items) = node.min_items {
        parts.push(format!("至少 {items} 项"));
    }
    if let Some(items) = node.max_items {
        parts.push(format!("至多 {items} 项"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// 区间的一段：`≤ 65535` / `< 0`（`upper` = 是否画在右侧）。
fn range_op(bound: f64, exclusive: bool, upper: bool) -> String {
    let op = match (upper, exclusive) {
        (true, false) => "≤",
        (true, true) => "<",
        (false, false) => "≥",
        (false, true) => ">",
    };
    format!("{op} {}", number_text(bound))
}

/// 数字的展示：整值去掉小数点（65535 而不是 65535.0），其余保持原样。
fn number_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// JSON 值的紧凑展示（默认值 / 示例）。
fn json_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Bool(true) => "enabled".to_string(),
        serde_json::Value::Bool(false) => "disabled".to_string(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Number(number) => number
            .as_f64()
            .map_or_else(|| number.to_string(), number_text),
        serde_json::Value::Null => "(null)".to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::Root;
    use crate::shared::panels::settings::ValueText;
    use crate::ui::settings::test_support as fx;
    use wing_api_client::models::SettingKind;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    fn row(path: &str, label: &str) -> Row {
        Row {
            root: Root::Gateway,
            path: path.to_string(),
            depth: 1,
            label: label.to_string(),
            value: ValueText::None,
            markers: fx::markers(),
            action: crate::shared::panels::settings::RowAction::Edit(
                crate::shared::panels::settings::ScalarKind::Str,
            ),
            match_spans: Vec::new(),
            tpath: path.to_string(),
        }
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_pane_carries_doc_notes_default_apply_and_path() {
        let catalog = fx::sample_catalog();
        let node = catalog
            .node_at("providers[].timeout_first_chunk")
            .expect("节点");
        let lines = detail_lines(
            Some(node),
            &row("providers[0].timeout_first_chunk", "timeout_first_chunk"),
            &palette(),
            60,
            20,
        );
        let joined = texts(&lines).join("\n");
        assert!(joined.contains("timeout_first_chunk"), "{joined}");
        assert!(joined.contains("流式首块超时"), "{joined}");
        assert!(joined.contains("响应头"), "{joined}");
        assert!(joined.contains("默认"), "{joined}");
        assert!(joined.contains("300"), "{joined}");
        assert!(joined.contains("生效"), "{joined}");
        assert!(joined.contains("热重载"), "{joined}");
        assert!(joined.contains("路径"), "{joined}");
        assert!(
            joined.contains("providers[0].timeout_first_chunk"),
            "{joined}"
        );
    }

    #[test]
    fn long_content_is_wrapped_and_clipped_to_the_height() {
        let catalog = fx::sample_catalog();
        let node = catalog
            .node_at("providers[].timeout_first_chunk")
            .expect("节点");
        for height in [1usize, 3, 8] {
            let lines = detail_lines(
                Some(node),
                &row("providers[0].timeout_first_chunk", "t"),
                &palette(),
                24,
                height,
            );
            assert!(lines.len() <= height, "height {height}: {}", lines.len());
            for line in &lines {
                let width = UnicodeWidthStr::width(line.to_string().as_str());
                assert!(width <= 24, "width: {width}");
            }
        }
    }

    #[test]
    fn choices_list_each_value_with_its_meaning() {
        let catalog = fx::sample_catalog();
        let node = catalog.node_at("providers[].protocol").expect("节点");
        let lines = detail_lines(
            Some(node),
            &row("providers[0].protocol", "protocol"),
            &palette(),
            80,
            20,
        );
        let joined = texts(&lines).join("\n");
        assert!(joined.contains("可选值"), "{joined}");
        assert!(joined.contains("openai"), "{joined}");
        assert!(
            joined.contains("OpenAI 兼容协议（/chat/completions）"),
            "{joined}"
        );
        assert!(joined.contains("anthropic"), "{joined}");
    }

    #[test]
    fn constraints_are_summarised_from_the_declaration() {
        let catalog = fx::sample_catalog();
        let port = catalog.node_at("gateway.port").expect("节点");
        assert_eq!(
            constraints_text(port).as_deref(),
            Some("值 ≥ 1 · 值 ≤ 65535")
        );
        let models = catalog.node_at("providers[].models").expect("节点");
        assert_eq!(constraints_text(models).as_deref(), Some("至少 1 项"));
        let name = catalog.node_at("providers[].name").expect("节点");
        assert_eq!(constraints_text(name), None);
    }

    #[test]
    fn exclusive_bounds_and_patterns_use_the_right_wording() {
        let mut node = fx::int_field("f", Some(0.0), Some(10.0));
        node.exclusive_min = true;
        node.exclusive_max = true;
        node.pattern = Some("^[a-z]+$".into());
        node.min_length = Some(3);
        let text = constraints_text(&node).expect("约束");
        assert!(text.contains("值 > 0"), "{text}");
        assert!(text.contains("值 < 10"), "{text}");
        assert!(text.contains("格式 ^[a-z]+$"), "{text}");
        assert!(text.contains("长度 ≥ 3"), "{text}");
    }

    #[test]
    fn a_missing_node_degrades_without_panicking() {
        let lines = detail_lines(
            None,
            &row("providers[0].models[0]", "ds-flash"),
            &palette(),
            30,
            10,
        );
        let joined = texts(&lines).join("\n");
        assert!(joined.contains("ds-flash"), "{joined}");
        assert!(joined.contains("无声明元信息"), "{joined}");
    }

    #[test]
    fn secrets_are_flagged_as_write_only() {
        let catalog = fx::sample_catalog();
        let node = catalog.node_at("providers[].api_key").expect("节点");
        let joined = texts(&detail_lines(
            Some(node),
            &row("providers[0].api_key", "api_key"),
            &palette(),
            30,
            12,
        ))
        .join("\n");
        assert!(joined.contains("只写不回显"), "{joined}");
    }

    #[test]
    fn the_doc_hint_line_falls_back_to_the_path_then_to_a_placeholder() {
        let catalog = fx::sample_catalog();
        let node = catalog.node_at("gateway.port").expect("节点");
        let hint = doc_hint_line(Some(node), &row("gateway.port", "port"), &palette(), 40);
        assert!(hint.to_string().contains("port 的说明"), "{hint:?}");

        let mut no_doc = fx::node("mystery", SettingKind::Str);
        no_doc.doc = String::new();
        let hint = doc_hint_line(Some(&no_doc), &row("mystery", "mystery"), &palette(), 40);
        assert_eq!(hint.to_string(), "mystery", "空 doc → 回落路径");

        let hint = doc_hint_line(None, &row("", "x"), &palette(), 40);
        assert_eq!(hint.to_string(), "(无说明)");
    }

    #[test]
    fn apply_labels_are_chinese_and_cover_unknown() {
        assert_eq!(apply_label(&ApplyScope::Hot), "热重载");
        assert_eq!(apply_label(&ApplyScope::NextSession), "新会话");
        assert_eq!(apply_label(&ApplyScope::Restart), "需重启网关");
        assert_eq!(apply_label(&ApplyScope::Unknown("odd".into())), "odd");
    }

    #[test]
    fn json_defaults_render_like_the_tree_does() {
        assert_eq!(json_text(&serde_json::json!(true)), "enabled");
        assert_eq!(json_text(&serde_json::json!(300.0)), "300");
        assert_eq!(json_text(&serde_json::json!("x")), "x");
        assert_eq!(json_text(&serde_json::json!(null)), "(null)");
        assert_eq!(
            json_text(&serde_json::json!({"a": 1})),
            "{\"a\":1}".to_string()
        );
    }
}
