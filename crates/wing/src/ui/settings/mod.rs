//! 设置面板的全屏 overlay（design §12–§14 的视觉落地）。
//!
//! 渲染层**零状态**：这里只有 `&SettingsPanel`、`&SettingNode` 与 `&ThemePalette` ——
//! 不持有面板、不调用它的任何 `&mut self` 方法、不发请求、不读盘。10 步骤负责
//! `frame.render_widget(Clear, area)` + 渲染本 widget + 把按键喂给 07 的状态机。
//!
//! ```text
//! ┌─ ⚙ Settings ── ~/.wing/core/config.yaml ── 3 unsaved ── valid ─┐
//! │ Gateway                                    │ timeout_first_chunk │
//! │   ▾ Providers (1)                          │                     │
//! │       ● timeout_first_chunk 120.0    hot   │ 流式首块超时（秒）  │
//! ├────────────────────────────────────────────┴─────────────────────┤
//! │ ↑↓ 移动 · ←→ 折叠/展开 · Enter 编辑 · a 新增 · d 删除 · r 复位   │
//! │ s 保存 · / 搜索 · p 问题(2) · Tab 切根 · ? 帮助 · Esc 关闭       │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # 接线契约（10 步骤照这份签名接）
//!
//! ```ignore
//! if let Some(panel) = &self.settings_panel {
//!     let area = frame.area();
//!     panel.set_viewport_rows(ui::settings::tree_viewport_rows(panel, area)); // 归 10（&mut）
//!     frame.render_widget(Clear, area);
//!     ui::settings::SettingsOverlay::new(
//!         panel,
//!         ui::settings::SettingsCatalogs::new(&self.settings_schema.root, self.interface_catalog.as_ref()),
//!         &palette,
//!     )
//!     .render(area, frame.buffer_mut());
//! }
//! ```
//!
//! - `catalogs` 必须与构造 `SettingsPanel` 用的是**同一份**（详情栏 / 色块预览要目录元信息，
//!   07 没有「行 → `SettingNode`」的访问器；见 08 design.md D11）。
//! - 任意 `Rect` 都能画；widget 自带 `Clear`，渲染进子区域也不会写到区域外。
//! - 颜色只取 `palette` 的六个槽位，不设背景（`terminal` 预设下一样成立）。

mod detail;
mod editor;
mod help;
mod problems;
mod prompt;
mod text;
mod tree;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::symbols::border;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Widget;
use wing_api_client::models::SettingNode;

use crate::config::ThemePalette;
use crate::render::markdown::truncate_left_to_display_width;
use crate::shared::panels::settings::Root;
use crate::shared::panels::settings::Row;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::View;

use text::clamp_line;
use text::display_width;
use text::pack_segments;

/// 宽屏断点（design §12.2）：`>= 110` 列时右侧详情栏出现。
pub const WIDE_MIN_WIDTH: u16 = 110;
/// 键位栏固定两行（design §12.2）。
const HINT_ROWS: u16 = 2;
/// 主体竖切比例（宽屏）：树 55% / 详情栏 45%，中间一条分隔列。
const TREE_SHARE: u16 = 55;

/// 渲染设置面板需要的目录视图。
///
/// 与 `SettingsPanel` 内部持有的 catalog 是同一份：`gateway` = `SettingsSchemaResponse.root`，
/// `interface` = 09 的 `interface_catalog()`（没注入 Interface 根时传 `None`）。
/// 它不是新数据源 —— 详情栏的 `doc` / `notes` / `example` / `choices` / 约束与色块预览的
/// `value_hint` 都住在目录节点上，而 07 的只读访问器里没有「行 → 节点」（design D11）。
pub struct SettingsCatalogs<'a> {
    /// Gateway 根的目录（`config.yaml`）。
    pub gateway: &'a SettingNode,
    /// Interface 根的目录（TUI 自己的配置）。
    pub interface: Option<&'a SettingNode>,
}

impl<'a> SettingsCatalogs<'a> {
    pub fn new(gateway: &'a SettingNode, interface: Option<&'a SettingNode>) -> Self {
        Self { gateway, interface }
    }

    /// 某个根的目录。
    pub(crate) fn catalog(&self, root: Root) -> Option<&'a SettingNode> {
        match root {
            Root::Gateway => Some(self.gateway),
            Root::Interface => self.interface,
        }
    }

    /// 一行对应的目录节点；解析不到就 `None`（合成行 / union 元素行是正常路径）。
    ///
    /// enum 选择项行的路径是合成的 `<enum>=<value>`，这里在 `=` 处回退到宿主节点 ——
    /// 色块预览要知道「这一列是颜色字段」。
    pub(crate) fn node_for(&self, root: Root, path: &str) -> Option<&'a SettingNode> {
        let catalog = self.catalog(root)?;
        let path = match path.rsplit_once('=') {
            Some((owner, _)) => owner,
            None => path,
        };
        catalog.node_at(path)
    }

    /// 这一行的值是不是「颜色」（`value_hint == "color"`，design §20 D25）。
    pub(crate) fn hint_for(&self, root: Root, path: &str) -> bool {
        self.node_for(root, path)
            .and_then(|node| node.value_hint.as_deref())
            == Some("color")
    }
}

/// 全屏设置 overlay（`Widget`；状态全在 [`SettingsPanel`] 里）。
pub struct SettingsOverlay<'a> {
    panel: &'a SettingsPanel,
    catalogs: SettingsCatalogs<'a>,
    palette: &'a ThemePalette,
}

impl<'a> SettingsOverlay<'a> {
    pub fn new(
        panel: &'a SettingsPanel,
        catalogs: SettingsCatalogs<'a>,
        palette: &'a ThemePalette,
    ) -> Self {
        Self {
            panel,
            catalogs,
            palette,
        }
    }

    /// 标题栏（上边框内嵌）：`⚙ Settings` + 路径 + 未保存数 + 有效性；搜索态整条替换。
    fn title_line(&self, width: usize) -> Line<'static> {
        let palette = self.palette;
        let dim = Style::default().fg(palette.dim);
        let label = Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD);
        let budget = width.saturating_sub(2); // 上边框两角

        if let Some(query) = self.panel.search_query() {
            // 长 query 左截断：光标在末尾，头部截断会连 `▏` 一起丢掉。
            let text = format!("搜索: {query}");
            let available = budget.saturating_sub(4); // `─ ` + `▏` + 尾空格
            let text = if display_width(&text) > available {
                truncate_left_to_display_width(&text, available)
            } else {
                text
            };
            return clamp_line(
                Line::from(vec![
                    Span::styled("─ ".to_string(), dim),
                    Span::styled(
                        format!("{text}▏"),
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]),
                budget,
            );
        }

        let mut segments: Vec<(String, Style)> = vec![
            ("⚙ Settings".to_string(), label),
            (self.panel.config_path().to_string(), dim),
        ];
        if self.panel.dirty_count() > 0 {
            segments.push((
                format!("{} unsaved", self.panel.dirty_count()),
                Style::default().fg(palette.warning),
            ));
        }
        let problems = self.panel.problems().len();
        if problems > 0 {
            segments.push((
                format!("{problems} problems"),
                Style::default()
                    .fg(palette.danger)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            segments.push(("valid".to_string(), Style::default().fg(palette.success)));
        }
        if self.panel.setup_mode() {
            segments.push((
                "setup mode".to_string(),
                Style::default().fg(palette.warning),
            ));
        }

        // 放不下就从右往左丢片段（`⚙ Settings` 是最后留下的那个）。
        while segments.len() > 1 && compose_width(&segments) > budget {
            segments.pop();
        }
        // 只剩片段 + 路径时，路径左截断（路径的区分度在尾段）。
        if segments.len() == 2 {
            let fixed = 2 + display_width(&segments[0].0) + 5;
            let path_budget = budget.saturating_sub(fixed);
            if path_budget < display_width(&segments[1].0) {
                segments[1].0 = truncate_left_to_display_width(&segments[1].0, path_budget.max(4));
            }
        }
        clamp_line(compose(&segments, palette), budget)
    }

    /// 键位栏的行（07 的 `footer_hint()` 是交互语义的唯一出处，这里只排版）。
    ///
    /// 按 ` · ` 整段装箱而不是逐字符折行：一个 CJK 断点就能把「Esc 关闭」挤到第 3 行、
    /// 然后被两行上限截掉 —— 而那是这个面板最重要的一个键。
    fn hint_lines(&self, rows: usize, width: usize) -> Vec<Line<'static>> {
        let dim = Style::default().fg(self.palette.dim);
        pack_segments(&self.panel.footer_hint(), width.saturating_sub(1), rows)
            .into_iter()
            .map(|row| clamp_line(Line::from(Span::styled(format!(" {row}"), dim)), width))
            .collect()
    }

    /// 主体：树（含详情栏 / doc 提示行）/ 问题清单。
    fn draw_body(&self, regions: &Regions, buf: &mut Buffer) {
        let body = regions.content;
        if body.height == 0 || body.width == 0 {
            return;
        }
        match self.panel.view() {
            View::Problems => {
                let lines = problems::problem_lines(
                    self.panel,
                    self.palette,
                    body.width as usize,
                    body.height as usize,
                );
                draw_lines(buf, body, &lines);
            }
            // `Help` 底下画树（A6：`help_back` 是私有的，浮层下面露出的树只是背景）。
            View::Tree | View::Help => self.draw_tree(regions, buf),
        }
    }

    /// 树视图：树行 / 竖分隔 / 详情栏（或窄屏的 doc 提示行）。
    fn draw_tree(&self, regions: &Regions, buf: &mut Buffer) {
        let palette = self.palette;
        let tree_area = regions.tree;
        if tree_area.height == 0 || tree_area.width == 0 {
            return;
        }
        let lines = tree::tree_lines(
            self.panel,
            &self.catalogs,
            palette,
            tree_area.width as usize,
            tree_area.height as usize,
        );
        draw_lines(buf, tree_area, &lines);

        // 竖分隔（宽屏）：整列都画，露出「两栏」的边界。
        if let Some(x) = regions.divider_x {
            let style = Style::default().fg(palette.dim);
            for y in tree_area.y..tree_area.bottom() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol("│").set_style(style);
                }
            }
        }

        let cursor_row = self.cursor_row();
        if let Some(detail) = regions.detail {
            let node = cursor_row.and_then(|row| self.catalogs.node_for(row.root, &row.path));
            let lines = match cursor_row {
                Some(row) => detail::detail_lines(
                    node,
                    row,
                    palette,
                    detail.width as usize,
                    detail.height as usize,
                ),
                None => Vec::new(),
            };
            draw_lines(buf, detail, &lines);
        } else if let Some(hint) = regions.doc_hint {
            let line = match cursor_row {
                Some(row) => detail::doc_hint_line(
                    self.catalogs.node_for(row.root, &row.path),
                    row,
                    palette,
                    hint.width as usize,
                ),
                None => Line::from(""),
            };
            buf.set_line(hint.x, hint.y, &line, hint.width);
        }
    }

    /// 当前光标行（空目录时 `None`）。
    fn cursor_row(&self) -> Option<&Row> {
        self.panel.rows().get(self.panel.cursor())
    }

    /// 帮助 / 模态提示浮层：居中、自带 `Clear`、按可用区域钳制。
    fn draw_overlays(&self, inner: Rect, buf: &mut Buffer) {
        if inner.width < 6 || inner.height < 3 {
            return;
        }
        if let Some(prompt) = self.panel.prompt() {
            let (width, height) = prompt::prompt_box_size(&prompt, self.palette, inner);
            let rect = text::centered_box(inner, width, height);
            let content = text::draw_box(rect, &prompt.title, buf, self.palette);
            let lines = prompt::prompt_lines(
                &prompt,
                self.palette,
                content.width.saturating_sub(1) as usize,
            );
            draw_lines(buf, content, &lines);
        }
        if self.panel.view() == View::Help {
            let width = inner.width.clamp(4, help::HELP_WIDTH + 2);
            let rect = text::centered_box(inner, width, inner.height);
            let content = text::draw_box(rect, "帮助", buf, self.palette);
            let lines = help::help_lines(
                self.palette,
                content.width.saturating_sub(1) as usize,
                content.height as usize,
            );
            draw_lines(buf, content, &lines);
        }
    }
}

impl Widget for SettingsOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 2 || area.height < 2 {
            return;
        }
        // 自带 Clear：10 也会清一次（design §12.1），这里保证 widget 单独渲染也不透底。
        Clear.render(area, buf);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::PLAIN)
            .border_style(Style::default().fg(self.palette.dim))
            .title(self.title_line(area.width as usize));
        let Some(regions) = Regions::new(area, self.panel.view(), self.panel.is_stale()) else {
            block.render(area, buf);
            return;
        };
        block.render(area, buf);

        // 过期横幅（design §18.4 第 6 条）：面板顶部一行 warning。
        if let Some(banner) = regions.banner {
            buf.set_line(
                banner.x,
                banner.y,
                &clamp_line(
                    Line::from(Span::styled(
                        "⚠ 配置已被其它客户端修改，按 R 重新载入".to_string(),
                        Style::default().fg(self.palette.warning),
                    )),
                    banner.width as usize,
                ),
                banner.width,
            );
        }

        self.draw_body(&regions, buf);

        // 分隔线（宽屏在竖分隔列上放一个 `┴` 交点）。
        if let Some(y) = regions.separator_y {
            let style = Style::default().fg(self.palette.dim);
            for x in area.x..area.right() {
                let symbol = if x == area.x {
                    "├"
                } else if x == area.right() - 1 {
                    "┤"
                } else if Some(x) == regions.divider_x {
                    "┴"
                } else {
                    "─"
                };
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol(symbol).set_style(style);
                }
            }
        }

        // 键位栏（07 的 footer_hint 折行；最多两行）。
        if regions.hints.height > 0 {
            let lines =
                self.hint_lines(regions.hints.height as usize, regions.hints.width as usize);
            draw_lines(buf, regions.hints, &lines);
        }

        self.draw_overlays(regions.inner, buf);
    }
}

/// 10 每帧用来告知面板可见行数：与 08 实际绘制主体内容的行数一致。
///
/// ```ignore
/// panel.set_viewport_rows(ui::settings::tree_viewport_rows(panel, frame.area()));
/// ```
pub fn tree_viewport_rows(panel: &SettingsPanel, area: Rect) -> u16 {
    Regions::new(area, panel.view(), panel.is_stale()).map_or(0, |regions| regions.content.height)
}

/// 区域切分（design §12.2）。
///
/// - 上边框 1 行（嵌标题）+ 下边框 1 行；
/// - 键位栏 2 行；分隔线 1 行（只在主体还有地方时出现）；
/// - 主体 = 其余部分；宽屏（`>= 110` 列）在树视图里竖切出详情栏，
///   窄屏树视图的末行留给当前行的 doc 提示；过期横幅占主体第一行。
struct Regions {
    /// 外框内部（浮层定位用）。
    inner: Rect,
    /// 主体内容（横幅与 doc 提示行之外的部分）。
    content: Rect,
    /// 树列（宽屏时是左侧 55%）。
    tree: Rect,
    /// 详情栏（仅宽屏树视图）。
    detail: Option<Rect>,
    /// 竖分隔列（仅宽屏树视图）。
    divider_x: Option<u16>,
    /// 窄屏的 doc 提示行（仅窄屏树视图）。
    doc_hint: Option<Rect>,
    /// 过期横幅。
    banner: Option<Rect>,
    /// 键位栏区域。
    hints: Rect,
    /// 分隔线的 y。
    separator_y: Option<u16>,
}

impl Regions {
    fn new(area: Rect, view: View, stale: bool) -> Option<Self> {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        if inner.width == 0 || inner.height == 0 {
            return None;
        }
        let hint_rows = HINT_ROWS.min(inner.height);
        let separator = inner.height > hint_rows;
        let body_height = inner
            .height
            .saturating_sub(hint_rows + u16::from(separator));
        let mut body = Rect::new(inner.x, inner.y, inner.width, body_height);
        let hints = Rect::new(inner.x, inner.bottom() - hint_rows, inner.width, hint_rows);
        let separator_y = separator.then(|| hints.y - 1);

        // 过期横幅：只在主体还能留下内容时出现（否则一行横幅会把树挤没）。
        let mut banner = None;
        if stale && body.height > 1 {
            banner = Some(Rect::new(body.x, body.y, body.width, 1));
            body = Rect::new(body.x, body.y + 1, body.width, body.height - 1);
        }
        // 窄屏树视图的最后一行是当前行的 doc 提示。
        let wide = area.width >= WIDE_MIN_WIDTH && view == View::Tree;
        let mut content = body;
        let mut doc_hint = None;
        if !wide && view != View::Problems && content.height > 1 {
            content.height -= 1;
            doc_hint = Some(Rect::new(body.x, body.y + content.height, body.width, 1));
        }

        let (tree, detail, divider_x) = if wide && content.width >= 3 {
            let tree_width = content.width * TREE_SHARE / 100;
            let x = content.x + tree_width;
            let tree = Rect::new(content.x, content.y, tree_width, content.height);
            let detail = Rect::new(
                x + 1,
                content.y,
                content.width - tree_width - 1,
                content.height,
            );
            (tree, Some(detail), Some(x))
        } else {
            (content, None, None)
        };

        Some(Self {
            inner,
            content,
            tree,
            detail,
            divider_x,
            doc_hint,
            banner,
            hints,
            separator_y,
        })
    }
}

/// 把一组行写进区域（每行都按区域宽度裁剪；超出的行丢弃）。
fn draw_lines(buf: &mut Buffer, area: Rect, lines: &[Line<'static>]) {
    for (index, line) in lines.iter().enumerate() {
        let y = area.y + index as u16;
        if y >= area.bottom() {
            break;
        }
        buf.set_line(area.x, y, line, area.width);
    }
}

/// 标题栏片段拼装：`─ <seg> ── <seg> … `。
fn compose(segments: &[(String, Style)], palette: &ThemePalette) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    let mut spans: Vec<Span<'static>> = vec![Span::styled("─ ".to_string(), dim)];
    for (index, (text, style)) in segments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ── ".to_string(), dim));
        }
        spans.push(Span::styled(text.clone(), *style));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// 片段拼装后的宽度（含前后缀）。
fn compose_width(segments: &[(String, Style)]) -> usize {
    let mut width = 3; // "─ " + 尾空格
    for (index, (text, _)) in segments.iter().enumerate() {
        if index > 0 {
            width += 4; // " ── "
        }
        width += display_width(text);
    }
    width
}
