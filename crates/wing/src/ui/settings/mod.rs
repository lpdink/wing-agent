//! 设置面板的浮层 overlay（v2：VS Code 式的居中卡片 + 双栏）。
//!
//! 渲染层**零状态**：这里只有 `&SettingsPanel`、`&SettingNode` 与 `&ThemePalette` ——
//! 不持有面板、不调用它的任何 `&mut self` 方法、不发请求、不读盘。10 步骤负责
//! 算出卡片矩形（[`card_area`]）、渲染本 widget、把按键喂给 07 的状态机。
//!
//! ```text
//!        ┌─ ⚙ Settings ── ~/.wing/core/config.yaml ── 2 unsaved ─ valid ─┐
//!        │ / 搜索…                                                        │
//!        ├───────────────┬────────────────────────────────────────────────┤
//!        │ Providers     │ Providers · LLM provider 与模型目录            │
//!        │ Agents        │   ▾ providers (2)                              │
//!        │ Behavior      │     ▸ default · openai                         │
//!        │ Images        │     ▸ dashscope · anthropic                    │
//!        │ Sessions      │     (+ 新增一项)                               │
//!        │ Gateway       │                                                │
//!        │ Advanced      │                                                │
//!        │ Interface     │                                                │
//!        ├───────────────┴────────────────────────────────────────────────┤
//!        │ ↑↓ 选分组 · Enter 进右栏 · Tab/←→ 切栏 · s 保存 · ? 帮助 · Esc │
//!        └────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # 几何（v2 的产品决策，逐条落地）
//!
//! 1. **浮层**：卡片 = `min(终端宽-4, 110) × min(终端高-4, 32)`，居中；四周露出聊天背景
//!    （所以 widget 只 `Clear` 自己那块，10 也不再全屏 `Clear`）。
//! 2. **退化**：终端 `< 80×24` 时铺满（浮层比全屏更难读的尺寸就别浮了）。
//! 3. **左栏**宽度 = `clamp(内容宽 × 22%, 14, 22)`，且保证右栏至少 24 列。
//! 4. **右栏** ≥ 78 列时再竖切出详情栏（58% / 42%）；否则末行留给当前行的 doc 提示。
//!
//! # 接线契约（10 步骤照这份签名接）
//!
//! ```ignore
//! if let Some(panel) = &self.settings_panel {
//!     let card = ui::settings::card_area(frame.area());
//!     panel.set_viewport_rows(ui::settings::tree_viewport_rows(panel, card) as usize);
//!     panel.set_anchor_viewport_rows(ui::settings::anchors_viewport_rows(panel, card) as usize);
//!     ui::settings::SettingsOverlay::new(panel, catalogs, &palette).render(card, frame.buffer_mut());
//! }
//! ```
//!
//! - `catalogs` 必须与构造 `SettingsPanel` 用的是**同一份**（详情栏 / 色块预览要目录元信息，
//!   07 没有「行 → `SettingNode`」的访问器；见 08 design.md D11）。
//! - 任意 `Rect` 都能画；widget 自带 `Clear`（只清自己那块），渲染进子区域也不会写到区域外。
//! - 颜色只取 `palette` 的槽位，不设背景（`terminal` 预设下一样成立）。

mod anchors;
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
use crate::shared::panels::settings::Focus;
use crate::shared::panels::settings::Root;
use crate::shared::panels::settings::Row;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::View;

use text::clamp_line;
use text::display_width;
use text::pack_segments;
use text::single_row;

// ── 几何常量（产品决策，见模块文档） ─────────────────────────

/// 浮层的最大尺寸。
pub const CARD_MAX_WIDTH: u16 = 110;
pub const CARD_MAX_HEIGHT: u16 = 32;
/// 浮层四周至少露出这么多背景（每边 2 列 / 2 行）。
const CARD_MARGIN: u16 = 4;
/// 小于这个终端尺寸就铺满（浮层反而更难读）。
const FLOAT_MIN_WIDTH: u16 = 80;
const FLOAT_MIN_HEIGHT: u16 = 24;
/// 右栏 ≥ 这个宽度才竖切出详情栏。
const DETAIL_MIN_WIDTH: u16 = 78;
/// 键位栏的行数上限（实际行数按内容算：装得下就一行，把省下的行还给主体）。
const HINT_ROWS_MAX: u16 = 2;
/// 左栏宽度：内容宽的 22%，钳在 [14, 22]。
const ANCHOR_SHARE: u16 = 22;
const ANCHOR_MIN_WIDTH: u16 = 14;
const ANCHOR_MAX_WIDTH: u16 = 22;
/// 右栏的最小可用宽度（左栏再宽也不能把右栏挤没）。
const RIGHT_MIN_WIDTH: u16 = 24;
/// 右栏内部：树 58% / 详情栏 42%。
const TREE_SHARE: u16 = 58;

/// 卡片矩形（浮层几何的唯一实现；10 与测试共用）。
///
/// `terminal` 是整个终端（或 setup 屏给面板的那块区域）：卡片在其中居中，
/// 终端小于 `80×24` 时退化为铺满 `terminal`。
pub fn card_area(terminal: Rect) -> Rect {
    if terminal.width < FLOAT_MIN_WIDTH || terminal.height < FLOAT_MIN_HEIGHT {
        return terminal;
    }
    let width = (terminal.width.saturating_sub(CARD_MARGIN)).min(CARD_MAX_WIDTH);
    let height = (terminal.height.saturating_sub(CARD_MARGIN)).min(CARD_MAX_HEIGHT);
    text::centered_box(terminal, width, height)
}

/// 渲染设置面板需要的目录视图。
///
/// 与 `SettingsPanel` 内部持有的 catalog 是同一份：`gateway` = `SettingsSchemaResponse.root`，
/// `interface` = 09 的 `interface_catalog()`（没注入 Interface 根时传 `None`）。
/// 它不是新数据源 —— 详情栏的 `doc` / `notes` / `example` / `choices` / 约束与色块预览的
/// `value_hint` 都住在目录节点上，而 07 的只读访问器里没有「行 → 节点」（design D11）。
#[derive(Clone, Copy)]
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

/// 设置面板 overlay（`Widget`；状态全在 [`SettingsPanel`] 里）。
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

    /// 标题栏（上边框内嵌）：`⚙ Settings` + 路径 + 未保存数 + 有效性。
    fn title_line(&self, width: usize) -> Line<'static> {
        let palette = self.palette;
        let dim = Style::default().fg(palette.dim);
        let label = Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD);
        let budget = width.saturating_sub(2); // 上边框两角

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

    /// 标题栏下面那一行：过期横幅 > 搜索行 > `/ 搜索…` 占位。
    ///
    /// v1 把搜索画在标题栏里（整条替换）；v2 的浮层有一行富余，搜索因此有了固定的家，
    /// 标题栏的路径 / 未保存数 / 有效性也就不会在输入 query 时集体消失。
    fn subheader_line(&self, width: usize) -> Line<'static> {
        let palette = self.palette;
        if self.panel.is_stale() {
            return clamp_line(
                Line::from(Span::styled(
                    " ⚠ 配置已被其它客户端修改，按 R 重新载入".to_string(),
                    Style::default().fg(palette.warning),
                )),
                width,
            );
        }
        if let Some(query) = self.panel.search_query() {
            // 长 query 左截断：光标在末尾，头部截断会连 `▏` 一起丢掉。
            let text = format!("搜索: {query}");
            let tail = format!("  {} 命中", self.panel.search_hits());
            let available = width.saturating_sub(2 + display_width(&tail) + 1);
            let text = if display_width(&text) > available {
                truncate_left_to_display_width(&text, available)
            } else {
                text
            };
            return clamp_line(
                Line::from(vec![
                    Span::styled(
                        format!(" {text}▏"),
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(tail, Style::default().fg(palette.dim)),
                ]),
                width,
            );
        }
        clamp_line(
            Line::from(Span::styled(
                " / 搜索…".to_string(),
                Style::default().fg(palette.dim),
            )),
            width,
        )
    }

    /// 右栏的第一行：当前分组的名字与说明（锚点的"详情"）。
    fn group_header_line(&self, width: usize) -> Line<'static> {
        let palette = self.palette;
        let Some(group) = self.panel.group() else {
            return Line::from("");
        };
        let title = Span::styled(
            group.title.clone(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        );
        // 标题 + ` · ` 之后全给说明（说明放不下就截断，标题不许被挤掉）。
        let budget = width.saturating_sub(2 + display_width(&group.title) + 3);
        let doc = Span::styled(
            single_row(group.doc.as_str(), budget),
            Style::default().fg(palette.dim),
        );
        clamp_line(
            Line::from(vec![Span::raw(" "), title, Span::raw(" · "), doc]),
            width,
        )
    }

    /// 键位栏要几行（1 或 2）：与 [`Self::hint_lines`] 同一份装箱数学，先算行数再切区域。
    fn hint_rows(&self, width: usize) -> u16 {
        let rows = pack_segments(
            &self.panel.footer_hint(),
            width.saturating_sub(1),
            HINT_ROWS_MAX as usize,
        )
        .len();
        (rows as u16).clamp(1, HINT_ROWS_MAX)
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

    /// 主体：左栏锚点 + 竖分隔 + 右栏（树 / 问题清单）。
    fn draw_body(&self, regions: &Regions, buf: &mut Buffer) {
        let body = regions.body;
        if body.height == 0 || body.width == 0 {
            return;
        }
        // 左栏锚点（问题清单视图里也在：它是导航，不是树的一部分）。
        if let Some(area) = regions.anchors {
            let lines = anchors::anchor_lines(
                self.panel,
                self.palette,
                area.width as usize,
                area.height as usize,
            );
            draw_lines(buf, area, &lines);
        }
        if let Some(x) = regions.anchor_divider_x {
            draw_vline(buf, x, body.y, body.bottom(), self.palette);
        }
        let Some(right) = regions.right else {
            return;
        };
        if right.height == 0 || right.width == 0 {
            return;
        }
        // 右栏第一行：分组头（问题清单视图里换成清单自己的标题）。
        if let Some(header) = regions.group_header {
            let line = match self.panel.view() {
                View::Problems => problems::heading_line(self.palette, header.width as usize),
                _ => self.group_header_line(header.width as usize),
            };
            buf.set_line(header.x, header.y, &line, header.width);
        }
        match self.panel.view() {
            View::Problems => {
                if let Some(area) = regions.items {
                    let lines = problems::problem_lines(
                        self.panel,
                        self.palette,
                        area.width as usize,
                        area.height as usize,
                    );
                    draw_lines(buf, area, &lines);
                }
            }
            // `Help` 底下画树（A6：`help_back` 是私有的，浮层下面露出的树只是背景）。
            View::Tree | View::Help => self.draw_items(regions, buf),
        }
    }

    /// 右栏的树：树行 / 竖分隔 / 详情栏（或窄屏的 doc 提示行）。
    fn draw_items(&self, regions: &Regions, buf: &mut Buffer) {
        let palette = self.palette;
        let Some(tree_area) = regions.items else {
            return;
        };
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

        // 竖分隔（有详情栏时）：整列都画，露出「两栏」的边界。
        if let Some(x) = regions.detail_divider_x {
            draw_vline(buf, x, tree_area.y, tree_area.bottom(), palette);
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
        // 自带 Clear（只清卡片这块）：浮层四周的聊天背景因此留着，卡片底下不透底。
        Clear.render(area, buf);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::PLAIN)
            .border_style(Style::default().fg(if self.panel.focus() == Focus::Groups {
                self.palette.accent
            } else {
                self.palette.dim
            }))
            .title(self.title_line(area.width as usize));
        let hint_rows = self.hint_rows(area.width.saturating_sub(2) as usize);
        let columns = !self.panel.anchors().is_empty();
        let Some(regions) = Regions::new(area, self.panel.view(), hint_rows, columns) else {
            block.render(area, buf);
            return;
        };
        block.render(area, buf);

        if let Some(subheader) = regions.subheader {
            let line = self.subheader_line(subheader.width as usize);
            buf.set_line(subheader.x, subheader.y, &line, subheader.width);
        }

        self.draw_body(&regions, buf);

        // 分隔线（键位栏上方；有左栏时在竖分隔列上放一个 `┴` 交点）。
        if let Some(y) = regions.separator_y {
            let style = Style::default().fg(self.palette.dim);
            for x in area.x..area.right() {
                let symbol = if x == area.x {
                    "├"
                } else if x == area.right() - 1 {
                    "┤"
                } else if Some(x) == regions.anchor_divider_x {
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

/// 10 每帧用来告知面板可见行数：与 08 实际绘制右栏内容的行数一致。
///
/// `area` 是**卡片**矩形（[`card_area`] 的产物），不是整个终端。
pub fn tree_viewport_rows(panel: &SettingsPanel, area: Rect) -> u16 {
    regions_for(panel, area).map_or(0, |regions| regions.items.map_or(0, |items| items.height))
}

/// 左栏的可见行数（锚点的翻页步长）。
pub fn anchors_viewport_rows(panel: &SettingsPanel, area: Rect) -> u16 {
    regions_for(panel, area).map_or(0, |regions| {
        regions.anchors.map_or(0, |anchors| anchors.height)
    })
}

/// 与 [`SettingsOverlay::render`] 同一份区域切分（翻页步长必须与实际画出来的行数一致）。
fn regions_for(panel: &SettingsPanel, area: Rect) -> Option<Regions> {
    let hint_rows = {
        let width = area.width.saturating_sub(2) as usize;
        let rows = pack_segments(
            &panel.footer_hint(),
            width.saturating_sub(1),
            HINT_ROWS_MAX as usize,
        )
        .len();
        (rows as u16).clamp(1, HINT_ROWS_MAX)
    };
    Regions::new(area, panel.view(), hint_rows, !panel.anchors().is_empty())
}

/// 区域切分（几何规则见模块文档）。
struct Regions {
    /// 外框内部（浮层定位用）。
    inner: Rect,
    /// 标题栏下面那一行（搜索 / 横幅 / 占位）。
    subheader: Option<Rect>,
    /// 主体（左栏 + 右栏）。
    body: Rect,
    /// 左栏锚点。
    anchors: Option<Rect>,
    /// 左栏与右栏之间的竖分隔列。
    anchor_divider_x: Option<u16>,
    /// 右栏整体。
    right: Option<Rect>,
    /// 右栏第一行（分组头 / 问题清单标题）。
    group_header: Option<Rect>,
    /// 右栏的行区（树 / 问题清单；宽屏时是左半部分）。
    items: Option<Rect>,
    /// 详情栏（仅宽屏树视图）。
    detail: Option<Rect>,
    /// 树与详情栏之间的竖分隔列。
    detail_divider_x: Option<u16>,
    /// 窄屏的 doc 提示行（仅窄屏树视图）。
    doc_hint: Option<Rect>,
    /// 键位栏区域。
    hints: Rect,
    /// 分隔线的 y。
    separator_y: Option<u16>,
}

impl Regions {
    /// `columns` = 有没有左栏（没有锚点时右栏独占，不留一条空白列）。
    fn new(area: Rect, view: View, hint_rows_max: u16, columns: bool) -> Option<Self> {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        if inner.width == 0 || inner.height == 0 {
            return None;
        }
        let mut rest = inner;

        // 标题栏下面那一行：搜索态 / 横幅优先（矮终端下也不能丢），其余情况留足主体。
        let subheader = (rest.height >= 6).then(|| {
            let row = Rect::new(rest.x, rest.y, rest.width, 1);
            rest = Rect::new(rest.x, rest.y + 1, rest.width, rest.height - 1);
            row
        });

        // 键位栏（含它上面那条分隔线）。
        let hint_rows = hint_rows_max.min(HINT_ROWS_MAX).min(rest.height);
        let separator = rest.height > hint_rows;
        let body_height = rest.height.saturating_sub(hint_rows + u16::from(separator));
        let body = Rect::new(rest.x, rest.y, rest.width, body_height);
        let hints = Rect::new(rest.x, rest.bottom() - hint_rows, rest.width, hint_rows);
        let separator_y = separator.then(|| hints.y - 1);

        // 左栏 + 竖分隔 + 右栏。
        let (anchors, anchor_divider_x, right) = split_columns(body, columns);

        // 右栏内部：分组头 1 行 + 行区（宽屏再竖切出详情栏）+ 窄屏的 doc 提示行。
        let (group_header, items, detail, detail_divider_x, doc_hint) = match right {
            Some(right) if right.height > 0 && right.width > 0 => {
                let mut rest = right;
                let header = (rest.height >= 4).then(|| {
                    let row = Rect::new(rest.x, rest.y, rest.width, 1);
                    rest = Rect::new(rest.x, rest.y + 1, rest.width, rest.height - 1);
                    row
                });
                let wide = rest.width >= DETAIL_MIN_WIDTH && view == View::Tree;
                let mut content = rest;
                let hint = if !wide && view != View::Problems && content.height > 1 {
                    let row = Rect::new(
                        content.x,
                        content.bottom().saturating_sub(1),
                        content.width,
                        1,
                    );
                    content.height = content.height.saturating_sub(1);
                    Some(row)
                } else {
                    None
                };
                if wide && content.width >= 3 {
                    let tree_width = content.width * TREE_SHARE / 100;
                    let x = content.x + tree_width;
                    (
                        header,
                        Some(Rect::new(content.x, content.y, tree_width, content.height)),
                        Some(Rect::new(
                            x + 1,
                            content.y,
                            content.width - tree_width - 1,
                            content.height,
                        )),
                        Some(x),
                        None,
                    )
                } else {
                    (header, Some(content), None, None, hint)
                }
            }
            _ => (None, None, None, None, None),
        };

        Some(Self {
            inner,
            subheader,
            body,
            anchors,
            anchor_divider_x,
            right,
            group_header,
            items,
            detail,
            detail_divider_x,
            doc_hint,
            hints,
            separator_y,
        })
    }
}

/// 主体竖切：左栏锚点 + 一条分隔列 + 右栏。
///
/// 两种情况**不出左栏**（右栏独占）：① 没有锚点（空目录）；② 太窄（放不下左栏 +
/// [`RIGHT_MIN_WIDTH`]）——锚点看不见了但设置项还能改（`Tab` 依旧能切焦点，
/// 只是没有可视的列，比挤成一团强）。
fn split_columns(body: Rect, columns: bool) -> (Option<Rect>, Option<u16>, Option<Rect>) {
    if !columns || body.width < ANCHOR_MIN_WIDTH + 1 + RIGHT_MIN_WIDTH || body.height == 0 {
        return (None, None, Some(body));
    }
    let share = body.width * ANCHOR_SHARE / 100;
    let mut left = share.clamp(ANCHOR_MIN_WIDTH, ANCHOR_MAX_WIDTH);
    // 右栏优先：左栏再宽也不能把右栏压到最小宽度以下。
    while left > ANCHOR_MIN_WIDTH && body.width.saturating_sub(left + 1) < RIGHT_MIN_WIDTH {
        left -= 1;
    }
    let x = body.x + left;
    (
        Some(Rect::new(body.x, body.y, left, body.height)),
        Some(x),
        Some(Rect::new(
            x + 1,
            body.y,
            body.width.saturating_sub(left + 1),
            body.height,
        )),
    )
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

/// 一条竖分隔线（整列都画）。
fn draw_vline(buf: &mut Buffer, x: u16, from: u16, to: u16, palette: &ThemePalette) {
    let style = Style::default().fg(palette.dim);
    for y in from..to {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_symbol("│").set_style(style);
        }
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
