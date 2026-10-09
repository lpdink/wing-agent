//! 树模型：一次扁平化（`flatten`）与唯一的显示值口径（`display_value`）。
//!
//! 两者都是**纯函数**：不读面板状态、不改任何东西，输入是（目录, 文档, 展开集, 问题, 搜索, 选择项），
//! 输出是「一行可见节点」的向量。每次状态变化后整树重算（≤ 几百节点，微秒级），
//! 因此「可见行」永远与状态一致，不存在增量维护的漂移（design §11.2）。
//!
//! 三个约定（08 渲染前必须知道）：
//!
//! 1. **列表项行的主文本在 `label`**（标量项 = 值本身；对象项 = `name · protocol · …` 摘要行），
//!    `value` 恒为 `ValueText::None`；字段行相反：`label` = 名字、`value` = 值。
//! 2. 合成行的 `path` 也是合成的：`(+ 新增一项)` 行是 `<列表>[]`，enum 选择项行是
//!    `<enum>=<value>`；它们不可能与真实路径相撞（`+` / `=` 不是路径名字符），也永远不会
//!    出现在 dirty / expanded 里。
//! 3. 搜索态下「命中」按 **catalog 模板路径**判定（`providers[].api_key`），每行的模板路径
//!    在 `tpath` 里（`pub(crate)`，只有本包用）。

use std::collections::HashSet;
use std::ops::Range;

use serde_json::Value;
use wing_api_client::models::ApplyScope;
use wing_api_client::models::PathStep;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;
use wing_api_client::models::parse_path;

use crate::config::catalog::secret_hint;

use super::ChoiceState;
use super::Problem;
use super::SearchFilter;
use super::doc::Root;
use super::doc::SettingsDoc;
use super::doc::index_path;
use super::doc::join_path;
use super::doc::path_is_within;
use super::edit::ScalarKind;
use super::edit::editor_kind;
use super::edit::fmt_f64;

/// 一行可见的树节点（设计 §11.2）。
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// 这一行属于哪个根。
    pub root: Root,
    /// 规范路径（具体下标）；合成行的合成路径见模块文档。
    pub path: String,
    /// 缩进层级（根头行 = 0）。
    pub depth: usize,
    /// 主文本。
    pub label: String,
    /// 第二列的值（结构节点与列表项行为 [`ValueText::None`]）。
    pub value: ValueText,
    pub markers: RowMarkers,
    /// `Enter` 干什么。
    pub action: RowAction,
    /// 搜索命中片段（`label` 的字节区间，可多个）。
    pub match_spans: Vec<Range<usize>>,
    /// catalog 模板路径（搜索判定用）。
    pub(crate) tpath: String,
}

/// 值的展示形态（`display_value` 的输出，design §11.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueText {
    /// 标量的展示文本。
    Text(String),
    /// 密文：只给末 4 位 hint（`None` = 短密钥不给）。
    Masked { hint: Option<String> },
    /// 键缺席 → 展示声明默认值（08 灰显）。
    Default(String),
    /// 密文 null（保留磁盘现值）。
    Inherited,
    /// 结构节点无值。
    None,
}

/// 一行的标记位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMarkers {
    /// 自己或子孙被标脏（结构节点是汇总）。
    pub dirty: bool,
    /// 自己或子孙有问题（结构节点是汇总）。
    pub problem: bool,
    /// 声明为必填（={`RowAction::Edit`} 的行缺值时 08 可加 `(required)` 备注）。
    pub required: bool,
    pub secret: bool,
    /// 生效域（详情栏 / 行尾标记）。
    pub apply: ApplyScope,
}

/// `Enter` 的分派依据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowAction {
    /// object / list / 根头 / 对象列表项：展开 / 折叠。
    Expand,
    /// bool：切换。
    Toggle,
    /// enum：展开内联选择项。
    OpenChoices,
    /// 标量字段：打开内联编辑器。
    Edit(ScalarKind),
    /// `(+ 新增一项)` 行：`variants` = 元素形态数（>1 先进形态选择）。
    AddItem { variants: usize },
    /// enum 选择项行：`value = None` 是 `(unset)` 行；`selected` = 内项光标所在行。
    Choose {
        value: Option<String>,
        selected: bool,
    },
    /// `editable=false` / `apply=readonly` / 未知 kind：只读。
    ReadOnly,
}

/// 扁平化一棵根（含根头行）。
///
/// - `expanded`：按 `(根, 具体路径)` 记的展开集（根头行用空路径）；
/// - `problems`：已合并的问题表，只用于行标记的汇总；
/// - `filter`：搜索态（`None` = 完整树）；
/// - `choices`：当前展开的 enum 选择项（只对匹配的根与路径生效）。
pub fn flatten(
    root: Root,
    catalog: &SettingNode,
    doc: &SettingsDoc,
    expanded: &HashSet<(Root, String)>,
    problems: &[Problem],
    filter: Option<&SearchFilter>,
    choices: Option<&ChoiceState>,
) -> Vec<Row> {
    let ctx = Ctx {
        root,
        catalog,
        doc,
        expanded,
        problems,
        filter,
        choices,
    };
    let mut rows = Vec::new();
    rows.push(root_row(&ctx));
    let vis = ctx.visible("");
    if ctx.show_children("", "", vis) || ctx.is_hit("") {
        for child in &catalog.children {
            visit(&ctx, &mut rows, child, Place::root_child(&child.key));
        }
    }
    rows
}

/// 扁平化时共享的只读上下文。
struct Ctx<'a> {
    root: Root,
    catalog: &'a SettingNode,
    doc: &'a SettingsDoc,
    expanded: &'a HashSet<(Root, String)>,
    problems: &'a [Problem],
    filter: Option<&'a SearchFilter>,
    choices: Option<&'a ChoiceState>,
}

impl Ctx<'_> {
    /// 搜索态下这一行是否**自己**可见（命中或命中项的祖先）；无 filter 时一切可见。
    fn visible(&self, tpath: &str) -> bool {
        self.filter
            .is_none_or(|f| f.is_hit(self.root, tpath) || f.is_ancestor(self.root, tpath))
    }

    /// 这一行**自己**命中搜索词。
    fn is_hit(&self, tpath: &str) -> bool {
        self.filter.is_some_and(|f| f.is_hit(self.root, tpath))
    }

    /// 这一行要不要下钻：用户展开的（且不可见就不展开），或搜索态下命中项的祖先链。
    fn show_children(&self, tpath: &str, path: &str, vis: bool) -> bool {
        let expanded = self.expanded.contains(&(self.root, path.to_string()));
        match self.filter {
            None => expanded,
            Some(f) => f.is_ancestor(self.root, tpath) || (vis && expanded),
        }
    }

    fn markers(&self, node: &SettingNode, path: &str) -> RowMarkers {
        let problem = self.problems.iter().any(|p| {
            p.root == self.root
                && p.path
                    .as_deref()
                    .is_some_and(|problem_path| path_is_within(path, problem_path))
        });
        RowMarkers {
            dirty: self.doc.has_dirty_below(self.root, path),
            problem,
            required: node.required,
            secret: node.secret,
            apply: node.apply.clone(),
        }
    }

    fn spans(&self, label: &str) -> Vec<Range<usize>> {
        self.filter.map_or_else(Vec::new, |f| f.find_spans(label))
    }
}

fn root_row(ctx: &Ctx) -> Row {
    let label = ctx.root.label();
    Row {
        root: ctx.root,
        path: String::new(),
        depth: 0,
        label: label.to_string(),
        value: ValueText::None,
        markers: RowMarkers {
            dirty: ctx.doc.has_dirty_below(ctx.root, ""),
            problem: ctx
                .problems
                .iter()
                .any(|p| p.root == ctx.root && p.path.is_some()),
            required: false,
            secret: false,
            apply: ApplyScope::Hot,
        },
        action: RowAction::Expand,
        match_spans: ctx.filter.map_or_else(Vec::new, |f| f.find_spans(label)),
        tpath: String::new(),
    }
}

/// 一次访问的「位置」：具体路径 / 模板路径 / 深度 / 搜索态强制 / 所属列表。
struct Place<'a> {
    path: String,
    tpath: String,
    depth: usize,
    /// 搜索态下被「命中的父级」连带展示（只发这一行，除非它自己也是祖先链的一员）。
    forced: bool,
    /// 当这一行是列表项时，它所属的**列表节点**（`summary_fields` 声明在列表上，§13.4）。
    owner: Option<&'a SettingNode>,
}

impl<'a> Place<'a> {
    fn root_child(key: &str) -> Place<'static> {
        Place {
            path: key.to_string(),
            tpath: key.to_string(),
            depth: 1,
            forced: false,
            owner: None,
        }
    }

    fn child(&self, key: &str) -> Place<'a> {
        Place {
            path: join_path(&self.path, key),
            tpath: join_path(&self.tpath, key),
            depth: self.depth + 1,
            forced: self.forced,
            owner: None,
        }
    }

    /// 列表项的位置（`owner` = 所属列表节点，`summary_fields` 由它声明）。
    fn item(&self, index: usize, forced: bool, owner: &'a SettingNode) -> Place<'a> {
        Place {
            path: index_path(&self.path, index),
            tpath: format!("{}[]", self.tpath),
            depth: self.depth + 1,
            forced,
            owner: Some(owner),
        }
    }
}

/// 访问一个目录节点。
fn visit(ctx: &Ctx, rows: &mut Vec<Row>, node: &SettingNode, place: Place<'_>) {
    let vis = ctx.visible(&place.tpath);
    if !vis && !place.forced {
        return;
    }
    let as_item = place.owner.is_some();
    match node.kind {
        SettingKind::List => emit_list(ctx, rows, node, place, vis),
        SettingKind::Object => emit_object(ctx, rows, node, place, vis),
        _ => emit_leaf(ctx, rows, node, place, as_item),
    }
}

fn emit_object(ctx: &Ctx, rows: &mut Vec<Row>, node: &SettingNode, place: Place<'_>, vis: bool) {
    let label = match place.owner {
        Some(list) => summary_line(ctx.doc, ctx.root, node, &place.path, &list.summary_fields),
        None => node.display_label().to_string(),
    };
    rows.push(Row {
        root: ctx.root,
        path: place.path.clone(),
        depth: place.depth,
        label: label.clone(),
        value: ValueText::None,
        markers: ctx.markers(node, &place.path),
        action: RowAction::Expand,
        match_spans: ctx.spans(&label),
        tpath: place.tpath.clone(),
    });
    let forced_children = ctx.is_hit(&place.tpath);
    if ctx.show_children(&place.tpath, &place.path, vis) || forced_children {
        for child in &node.children {
            let mut child_place = place.child(&child.key);
            child_place.forced = forced_children;
            visit(ctx, rows, child, child_place);
        }
    }
}

fn emit_list(ctx: &Ctx, rows: &mut Vec<Row>, node: &SettingNode, place: Place<'_>, vis: bool) {
    let count = effective_len(ctx.doc, ctx.root, &place.path, node);
    rows.push(Row {
        root: ctx.root,
        path: place.path.clone(),
        depth: place.depth,
        label: format!("{} ({count})", node.display_label()),
        value: ValueText::None,
        markers: ctx.markers(node, &place.path),
        action: RowAction::Expand,
        match_spans: ctx.spans(node.display_label()),
        tpath: place.tpath.clone(),
    });
    let forced_items = ctx.is_hit(&place.tpath);
    if !ctx.show_children(&place.tpath, &place.path, vis) && !forced_items {
        return;
    }
    for index in 0..count {
        let item_place = place.item(index, forced_items, node);
        match concrete_node(ctx.catalog, ctx.doc, ctx.root, &item_place.path) {
            Some(item_node) => visit(ctx, rows, item_node, item_place),
            None => rows.push(item_fallback_row(ctx, &item_place.path, item_place.depth)),
        }
    }
    if ctx.filter.is_none() && add_row_visible(node, count) {
        rows.push(add_row(ctx, node, &place, count));
    }
}

/// 形态未知的列表项（目录与文档对不上时的只读兜底行）。
fn item_fallback_row(ctx: &Ctx, item_path: &str, depth: usize) -> Row {
    let value = ctx.doc.value(ctx.root, item_path);
    let label = value.map_or_else(|| "(?)".to_string(), compact_json);
    Row {
        root: ctx.root,
        path: item_path.to_string(),
        depth,
        label,
        value: ValueText::None,
        markers: RowMarkers {
            dirty: ctx.doc.has_dirty_below(ctx.root, item_path),
            problem: false,
            required: false,
            secret: false,
            apply: ApplyScope::Hot,
        },
        action: RowAction::ReadOnly,
        match_spans: Vec::new(),
        tpath: item_path.to_string(),
    }
}

fn add_row(ctx: &Ctx, node: &SettingNode, place: &Place<'_>, _count: usize) -> Row {
    Row {
        root: ctx.root,
        path: format!("{}[]", place.path),
        depth: place.depth + 1,
        label: "(+ 新增一项)".into(),
        value: ValueText::None,
        markers: RowMarkers {
            dirty: false,
            problem: false,
            required: false,
            secret: false,
            apply: node.apply.clone(),
        },
        action: RowAction::AddItem {
            variants: add_variants(node),
        },
        match_spans: Vec::new(),
        tpath: format!("{}[]", place.tpath),
    }
}

/// `(+ 新增一项)` 行要不要出现：可编辑、有元素形态、没到 `max_items`。
fn add_row_visible(node: &SettingNode, count: usize) -> bool {
    node.editable
        && node.apply != ApplyScope::Readonly
        && (node.element.is_some() || node.variants.is_some())
        && node.max_items.is_none_or(|max| (count as i64) < max)
}

fn add_variants(node: &SettingNode) -> usize {
    node.variants
        .as_ref()
        .map_or_else(|| usize::from(node.element.is_some()), Vec::len)
}

fn emit_leaf(ctx: &Ctx, rows: &mut Vec<Row>, node: &SettingNode, place: Place<'_>, as_item: bool) {
    // 标量列表项的主文本是**值本身**（design §13.4 的 `Bash` / `Read` 行）。
    let label = if as_item {
        ctx.doc
            .value(ctx.root, &place.path)
            .map_or_else(|| node.display_label().to_string(), render_scalar)
    } else {
        node.display_label().to_string()
    };
    let value = display_value(ctx.doc, ctx.root, node, &place.path);
    rows.push(Row {
        root: ctx.root,
        path: place.path.clone(),
        depth: place.depth,
        label,
        value,
        markers: ctx.markers(node, &place.path),
        action: leaf_action(node),
        match_spans: ctx.spans(node.display_label()),
        tpath: place.tpath.clone(),
    });
    // enum 的内联选择项（design §20 D19）：只在树里插子行，不是独立视图。
    if node.kind == SettingKind::Enum
        && let Some(state) = ctx
            .choices
            .filter(|state| state.root == ctx.root && state.path == place.path)
    {
        push_choice_rows(ctx, rows, node, &place, state);
    }
}

fn push_choice_rows(
    ctx: &Ctx,
    rows: &mut Vec<Row>,
    node: &SettingNode,
    place: &Place<'_>,
    state: &ChoiceState,
) {
    let mut push =
        |label: String, value: ValueText, choose: Option<String>, selected: bool, ok: String| {
            let spans = ctx.spans(&label);
            rows.push(Row {
                root: ctx.root,
                path: ok,
                depth: place.depth + 1,
                label,
                value,
                markers: RowMarkers {
                    dirty: false,
                    problem: false,
                    required: false,
                    secret: false,
                    apply: ApplyScope::Hot,
                },
                action: RowAction::Choose {
                    value: choose,
                    selected,
                },
                match_spans: spans,
                tpath: place.tpath.clone(),
            });
        };
    for (index, choice) in node.choices.iter().enumerate() {
        push(
            choice.value.clone(),
            choice
                .doc
                .as_ref()
                .filter(|doc| !doc.is_empty())
                .map_or(ValueText::None, |doc| ValueText::Text(doc.clone())),
            Some(choice.value.clone()),
            index == state.cursor,
            format!("{}={}", place.path, choice.value),
        );
    }
    if node.nullable {
        let label = "(unset)".to_string();
        let hint = match node.default.as_ref().and_then(Value::as_str) {
            Some(default) => format!("跟随默认 {default}"),
            None => "清除（跟随默认）".to_string(),
        };
        push(
            label,
            ValueText::Text(hint),
            None,
            state.cursor >= node.choices.len(),
            format!("{}=(unset)", place.path),
        );
    }
}

fn leaf_action(node: &SettingNode) -> RowAction {
    if !node.editable || node.apply == ApplyScope::Readonly {
        return RowAction::ReadOnly;
    }
    match node.kind {
        SettingKind::Bool => RowAction::Toggle,
        SettingKind::Enum => RowAction::OpenChoices,
        _ => editor_kind(node).map_or(RowAction::ReadOnly, RowAction::Edit),
    }
}

/// 有效项数：文档里有数组就用它，缺席时回落到声明默认里的数组（见 design A2 的说明），
/// 其余情况 = 0。
fn effective_len(doc: &SettingsDoc, root: Root, path: &str, node: &SettingNode) -> usize {
    match doc.value(root, path) {
        Some(Value::Array(items)) => items.len(),
        Some(_) => 0,
        None => node
            .default
            .as_ref()
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
    }
}

/// 唯一取值口径（design §11.3 的伪码 + 密文三态）。
pub fn display_value(doc: &SettingsDoc, root: Root, node: &SettingNode, path: &str) -> ValueText {
    // 结构节点没有「值」这一列（列表项行的文本走 label）。
    if node.kind == SettingKind::Object || node.kind == SettingKind::List {
        return ValueText::None;
    }
    match doc.value(root, path) {
        Some(Value::Null) if node.secret => match doc.secret_state(root, path) {
            Some(state) => match state.state {
                wing_api_client::models::SecretPresence::Set => ValueText::Masked {
                    hint: state.hint.clone(),
                },
                wing_api_client::models::SecretPresence::Empty => ValueText::Text("(empty)".into()),
                _ => ValueText::Text("(not set)".into()),
            },
            // 没有密文表（老网关 / Interface 本地文档）→ 原样保留。
            None => ValueText::Inherited,
        },
        Some(Value::Null) if node.nullable => ValueText::Text("(unset)".into()),
        Some(Value::String(text)) if node.secret => {
            if text.is_empty() {
                ValueText::Text("(empty)".into())
            } else {
                ValueText::Masked {
                    // 本地 Interface 密钥的 hint：与 `--dump-config` 的掩码注释共用一份实现
                    // （`config/catalog.rs::secret_hint`，规则与后端 `_state_of` 同源）。
                    hint: secret_hint(text),
                }
            }
        }
        Some(value) => ValueText::Text(render_scalar(value)),
        None if node.secret => match node.default.as_ref().filter(|_| node.has_default) {
            Some(default) => ValueText::Default(render_scalar(default)),
            None => ValueText::Text("(not set)".into()),
        },
        None => match node.default.as_ref().filter(|_| node.has_default) {
            Some(default) => ValueText::Default(render_scalar(default)),
            None if node.required => ValueText::Text("(required)".into()),
            None => ValueText::None,
        },
    }
}

/// 值的展示文本（bool 走 `enabled`/`disabled`，float 整值补一位小数，map 走紧凑 JSON）。
pub(crate) fn render_scalar(value: &Value) -> String {
    match value {
        Value::Bool(true) => "enabled".into(),
        Value::Bool(false) => "disabled".into(),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                int.to_string()
            } else if let Some(uint) = number.as_u64() {
                uint.to_string()
            } else {
                number.as_f64().map_or_else(|| number.to_string(), fmt_f64)
            }
        }
        Value::String(text) => text.clone(),
        Value::Null => "(null)".into(),
        other => compact_json(other),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// 按具体路径在目录里找节点（含 `variants` 列表：用文档里的实际值挑形态）。
///
/// `SettingNode::node_at` 对 union 元素的列表项返回 `None`（目录无法单独判定形态），
/// 面板需要「具体值 → 形态」的解析，因此这里在 `[i]` 处用 [`SettingNode::select_variant`]。
pub(crate) fn concrete_node<'a>(
    catalog: &'a SettingNode,
    doc: &SettingsDoc,
    root: Root,
    path: &str,
) -> Option<&'a SettingNode> {
    let steps = parse_path(path)?;
    let mut node = catalog;
    let mut walked = String::new();
    for step in &steps {
        match step {
            PathStep::Key(name) => {
                node = node.children.iter().find(|child| child.key == *name)?;
                walked = join_path(&walked, name);
            }
            PathStep::Index(index) => {
                walked = index_path(&walked, *index);
                node = match (node.element.as_deref(), node.variants.as_deref()) {
                    (Some(element), _) => element,
                    (None, Some(_)) => node.select_variant(doc.value(root, &walked)?)?,
                    _ => return None,
                };
            }
            PathStep::Element => return None,
        }
    }
    Some(node)
}

/// 列表项的摘要行（design §13.4）：`summary_fields` 指定的字段（声明在**列表节点**上），
/// 缺省回落第一个标量子字段。
pub(crate) fn summary_line(
    doc: &SettingsDoc,
    root: Root,
    item: &SettingNode,
    item_path: &str,
    summary_fields: &[String],
) -> String {
    let explicit: Vec<String> = if summary_fields.is_empty() {
        item.children
            .iter()
            .find(|child| summary_scalar(child) && !child.secret)
            .map(|child| vec![child.key.clone()])
            .unwrap_or_default()
    } else {
        summary_fields.to_vec()
    };
    let mut parts = Vec::new();
    for field in &explicit {
        let Some(child) = item.children.iter().find(|child| child.key == *field) else {
            continue;
        };
        let path = join_path(item_path, field);
        let text = match doc.value(root, &path) {
            Some(_) if child.secret => "••••".to_string(),
            Some(value) => render_scalar(value),
            None => match child.default.as_ref().filter(|_| child.has_default) {
                Some(value) => render_scalar(value),
                None => continue,
            },
        };
        parts.push(text);
    }
    if parts.is_empty()
        && let Some(part) = item
            .children
            .iter()
            .filter(|child| summary_scalar(child) && !child.secret)
            .find_map(|child| {
                doc.value(root, &join_path(item_path, &child.key))
                    .map(render_scalar)
            })
    {
        parts.push(part);
    }
    if parts.is_empty() {
        "(空)".into()
    } else {
        parts.join(" · ")
    }
}

fn summary_scalar(node: &SettingNode) -> bool {
    matches!(
        node.kind,
        SettingKind::Str
            | SettingKind::Int
            | SettingKind::Float
            | SettingKind::Bool
            | SettingKind::Enum
            | SettingKind::Secret
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::test_support as fx;
    use serde_json::json;
    use std::collections::HashMap;

    fn flatten_sample() -> Vec<Row> {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        let mut expanded = HashSet::new();
        expanded.insert((Root::Gateway, String::new()));
        flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None)
    }

    fn label(rows: &[Row], path: &str) -> String {
        rows.iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| {
                panic!(
                    "row {path} not found in {:?}",
                    rows.iter().map(|r| &r.path).collect::<Vec<_>>()
                )
            })
            .label
            .clone()
    }

    fn row<'a>(rows: &'a [Row], path: &str) -> &'a Row {
        rows.iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("row {path} not found"))
    }

    fn expand_all(base: &[(Root, &str)]) -> HashSet<(Root, String)> {
        base.iter().map(|(r, p)| (*r, (*p).to_string())).collect()
    }

    // ── 扁平化 ───────────────────────────────────────────────

    #[test]
    fn root_row_is_first_and_children_follow_in_declaration_order() {
        let rows = flatten_sample();
        assert_eq!(
            rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["", "providers", "gateway", "tools", "extra_body"],
            "根头行在前，其余按声明序（未展开）"
        );
        assert_eq!(rows[0].label, "Gateway");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].label, "providers (1)", "列表行带项数");
    }

    #[test]
    fn expanding_an_object_reveals_its_children_with_depth() {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        let expanded = expand_all(&[
            (Root::Gateway, ""),
            (Root::Gateway, "gateway"),
            (Root::Gateway, "gateway.auth"),
        ]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "",
                "providers",
                "gateway",
                "gateway.port",
                "gateway.auth",
                "gateway.auth.enabled",
                "tools",
                "extra_body"
            ]
        );
        assert_eq!(row(&rows, "gateway.port").depth, 2);
        assert_eq!(row(&rows, "gateway.auth.enabled").depth, 3);
    }

    #[test]
    fn collapsing_hides_the_whole_subtree() {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        let expanded = expand_all(&[(Root::Gateway, "")]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        assert_eq!(rows.len(), 5, "只展开根，全部顶层折起来");
    }

    #[test]
    fn list_items_render_summary_rows_and_the_add_row() {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        let expanded = expand_all(&[(Root::Gateway, ""), (Root::Gateway, "providers")]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        let item = row(&rows, "providers[0]");
        assert_eq!(item.label, "default · openai", "summary_fields 顺序");
        assert_eq!(item.value, ValueText::None, "列表项行主文本在 label");
        assert_eq!(item.action, RowAction::Expand);
        let add = row(&rows, "providers[]");
        assert_eq!(add.label, "(+ 新增一项)");
        assert_eq!(add.action, RowAction::AddItem { variants: 1 });
        assert_eq!(add.depth, item.depth);
    }

    #[test]
    fn scalar_list_items_show_the_value_in_the_label() {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        let expanded = expand_all(&[(Root::Gateway, ""), (Root::Gateway, "tools")]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        assert_eq!(label(&rows, "tools[0]"), "Bash");
        assert_eq!(label(&rows, "tools[1]"), "Read");
        assert_eq!(
            row(&rows, "tools[0]").action,
            RowAction::Edit(ScalarKind::Str)
        );
        assert_eq!(label(&rows, "tools[]"), "(+ 新增一项)");
    }

    #[test]
    fn union_list_items_take_their_shape_from_the_value() {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        let expanded = expand_all(&[
            (Root::Gateway, ""),
            (Root::Gateway, "providers"),
            (Root::Gateway, "providers[0]"),
            (Root::Gateway, "providers[0].models"),
        ]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        // 裸字符串形态 → 标量项行。
        assert_eq!(label(&rows, "providers[0].models[0]"), "ds-flash");
        assert_eq!(
            row(&rows, "providers[0].models[0]").action,
            RowAction::Edit(ScalarKind::Str)
        );
        assert_eq!(
            row(&rows, "providers[0].models[]").action,
            RowAction::AddItem { variants: 2 },
            "两个形态 → 先选形态"
        );
    }

    #[test]
    fn object_model_variant_renders_fields_after_expansion() {
        let catalog = fx::sample_catalog();
        let mut doc = fx::sample_doc();
        doc.set_value(
            Root::Gateway,
            "providers[0].models[1]",
            json!({"id": "ds-pro", "display_name": "DeepSeek Pro"}),
        );
        let expanded = expand_all(&[
            (Root::Gateway, ""),
            (Root::Gateway, "providers"),
            (Root::Gateway, "providers[0]"),
            (Root::Gateway, "providers[0].models"),
            (Root::Gateway, "providers[0].models[1]"),
        ]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        assert_eq!(
            label(&rows, "providers[0].models[1]"),
            "ds-pro",
            "summary_fields=[id]"
        );
        assert_eq!(
            label(&rows, "providers[0].models[1].display_name"),
            "display_name"
        );
        assert_eq!(
            row(&rows, "providers[0].models[1].display_name").value,
            ValueText::Text("DeepSeek Pro".into())
        );
    }

    #[test]
    fn empty_list_stays_visible_with_zero_items() {
        let catalog = fx::sample_catalog();
        let doc = fx::empty_doc();
        let expanded = expand_all(&[(Root::Gateway, ""), (Root::Gateway, "providers")]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        assert_eq!(label(&rows, "providers"), "providers (0)");
        assert_eq!(
            rows.iter()
                .filter(|r| r.path.starts_with("providers["))
                .count(),
            1,
            "只剩下 (+) 行"
        );
    }

    #[test]
    fn readonly_lists_get_no_add_row() {
        let mut catalog = fx::sample_catalog();
        // 把 tools 标成只读。
        catalog.children[2].editable = false;
        let doc = fx::sample_doc();
        let expanded = expand_all(&[(Root::Gateway, ""), (Root::Gateway, "tools")]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        assert!(
            rows.iter().all(|r| r.path != "tools[]"),
            "只读列表没有新增行"
        );
        assert_eq!(
            row(&rows, "tools[0]").action,
            RowAction::Edit(ScalarKind::Str),
            "项仍可看"
        );
    }

    #[test]
    fn max_items_hides_the_add_row() {
        let mut catalog = fx::sample_catalog();
        catalog.children[2].max_items = Some(2);
        let doc = fx::sample_doc();
        let expanded = expand_all(&[(Root::Gateway, ""), (Root::Gateway, "tools")]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        assert!(rows.iter().all(|r| r.path != "tools[]"), "满了就没有新增行");
    }

    #[test]
    fn sectionless_rows_keep_catalog_order_even_when_order_field_is_set() {
        let mut catalog = fx::sample_catalog();
        catalog.children[0].order = 9;
        catalog.children[1].order = -1;
        let rows = flatten(
            Root::Gateway,
            &catalog,
            &fx::empty_doc(),
            &expand_all(&[(Root::Gateway, "")]),
            &[],
            None,
            None,
        );
        assert_eq!(
            rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["", "providers", "gateway", "tools", "extra_body"],
            "声明序就是展示序"
        );
    }

    // ── 显示值 ───────────────────────────────────────────────

    #[test]
    fn display_value_covers_every_branch() {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        let port = &catalog.children[1].children[0];
        let name = &catalog.children[0].element.as_ref().unwrap().children[0];
        let api_key = &catalog.children[0].element.as_ref().unwrap().children[3];
        let enabled = &catalog.children[1].children[1].children[0];
        let extra_body = &catalog.children[3];

        assert_eq!(
            display_value(&doc, Root::Gateway, port, "gateway.port"),
            ValueText::Text("32523".into())
        );
        assert_eq!(
            display_value(&doc, Root::Gateway, name, "providers[0].name"),
            ValueText::Text("default".into())
        );
        assert_eq!(
            display_value(&doc, Root::Gateway, enabled, "gateway.auth.enabled"),
            ValueText::Text("enabled".into()),
            "bool 走语义文案"
        );
        // 缺席 + 有默认 → Default（灰显）。
        assert_eq!(
            display_value(&doc, Root::Gateway, &fx::int_field("x", None, None), "nope"),
            ValueText::Default("0".into())
        );
        // 缺席 + 必填 → (required)。
        assert_eq!(
            display_value(&doc, Root::Gateway, &fx::required_str("x"), "nope"),
            ValueText::Text("(required)".into())
        );
        // 结构无值。
        assert_eq!(
            display_value(&doc, Root::Gateway, &catalog.children[1], "gateway"),
            ValueText::None
        );
        // map 有值 → 紧凑 JSON。
        assert_eq!(
            display_value(&doc, Root::Gateway, extra_body, "extra_body"),
            ValueText::Text("{\"thinking\":{\"type\":\"enabled\"}}".into())
        );
        // 密文：文档里 null + 无表 → Inherited。
        assert_eq!(
            display_value(&doc, Root::Gateway, api_key, "providers[0].api_key"),
            ValueText::Inherited
        );
    }

    #[test]
    fn secret_three_states_render_with_hints() {
        let catalog = fx::sample_catalog();
        let api_key = &catalog.children[0].element.as_ref().unwrap().children[3];
        let path = "providers[0].api_key";

        let doc = fx::doc_with_secret_state();
        assert_eq!(
            display_value(&doc, Root::Gateway, api_key, path),
            ValueText::Masked {
                hint: Some("ab12".into())
            }
        );

        let mut empty = HashMap::new();
        empty.insert(
            path.to_string(),
            wing_api_client::models::SecretState {
                state: wing_api_client::models::SecretPresence::Empty,
                hint: None,
            },
        );
        let doc = SettingsDoc::new(fx::sample_gateway_doc(), json!({}), empty, "fp".into());
        assert_eq!(
            display_value(&doc, Root::Gateway, api_key, path),
            ValueText::Text("(empty)".into())
        );

        let mut absent = HashMap::new();
        absent.insert(
            path.to_string(),
            wing_api_client::models::SecretState {
                state: wing_api_client::models::SecretPresence::Absent,
                hint: None,
            },
        );
        let doc = SettingsDoc::new(fx::sample_gateway_doc(), json!({}), absent, "fp".into());
        assert_eq!(
            display_value(&doc, Root::Gateway, api_key, path),
            ValueText::Text("(not set)".into())
        );

        // 键整条缺席 → (not set)。
        let doc = fx::empty_doc();
        assert_eq!(
            display_value(&doc, Root::Gateway, api_key, path),
            ValueText::Text("(not set)".into())
        );
    }

    #[test]
    fn interface_secrets_are_masked_with_a_locally_computed_hint() {
        let catalog = fx::sample_catalog();
        let mut node = fx::secret_field("api_key");
        node.path = "api_key".into();
        let doc = SettingsDoc::new(
            json!({}),
            json!({"api_key": "sk-1234567890"}),
            HashMap::new(),
            "fp".into(),
        );
        assert_eq!(
            display_value(&doc, Root::Interface, &node, "api_key"),
            ValueText::Masked {
                hint: Some("7890".into())
            }
        );
        let short = SettingsDoc::new(
            json!({}),
            json!({"api_key": "sk-12"}),
            HashMap::new(),
            "fp".into(),
        );
        assert_eq!(
            display_value(&short, Root::Interface, &node, "api_key"),
            ValueText::Masked { hint: None },
            "短密钥不给 hint"
        );
        let _ = catalog;
    }

    #[test]
    fn nullable_enum_null_renders_as_unset() {
        let mut protocol = fx::enum_field("protocol", &[("openai", ""), ("anthropic", "")]);
        protocol.nullable = true;
        if let Some(choice) = protocol.choices.first_mut() {
            choice.doc = None;
        }
        let doc = SettingsDoc::new(
            json!({"protocol": null}),
            json!({}),
            HashMap::new(),
            "fp".into(),
        );
        assert_eq!(
            display_value(&doc, Root::Gateway, &protocol, "protocol"),
            ValueText::Text("(unset)".into())
        );
    }

    #[test]
    fn float_defaults_and_values_keep_a_decimal() {
        let mut node = fx::node("timeout", SettingKind::Float);
        node.has_default = true;
        node.default = Some(json!(300.0));
        let doc = fx::empty_doc();
        assert_eq!(
            display_value(&doc, Root::Gateway, &node, "timeout"),
            ValueText::Default("300.0".into())
        );
    }

    // ── 标记 ─────────────────────────────────────────────────

    #[test]
    fn markers_roll_up_dirty_and_problems_to_ancestors() {
        // 展开到 api_key / name 可见的那一层。
        let catalog = fx::sample_catalog();
        let mut doc = fx::sample_doc();
        doc.mark_dirty(Root::Gateway, "providers[0].api_key");
        let problems = vec![fx::problem(
            Root::Gateway,
            Some("providers[0].name"),
            "missing_required",
            "x",
        )];
        let expanded = expand_all(&[
            (Root::Gateway, ""),
            (Root::Gateway, "providers"),
            (Root::Gateway, "providers[0]"),
        ]);
        let rows = flatten(
            Root::Gateway,
            &catalog,
            &doc,
            &expanded,
            &problems,
            None,
            None,
        );
        assert!(row(&rows, "providers[0].api_key").markers.dirty);
        assert!(row(&rows, "providers[0]").markers.dirty, "项行是汇总");
        assert!(row(&rows, "providers").markers.dirty);
        assert!(row(&rows, "").markers.dirty);
        assert!(row(&rows, "providers").markers.problem);
        assert!(row(&rows, "providers[0].name").markers.problem);
        assert!(!row(&rows, "gateway").markers.problem);
    }

    #[test]
    fn markers_expose_required_secret_and_apply_scope() {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        let expanded = expand_all(&[
            (Root::Gateway, ""),
            (Root::Gateway, "providers"),
            (Root::Gateway, "providers[0]"),
        ]);
        let rows = flatten(Root::Gateway, &catalog, &doc, &expanded, &[], None, None);
        assert!(row(&rows, "providers[0].name").markers.required);
        assert!(row(&rows, "providers[0].api_key").markers.secret);
        assert_eq!(
            row(&rows, "providers[0].name").markers.apply,
            ApplyScope::Hot
        );
    }

    // ── 摘要行 ───────────────────────────────────────────────

    #[test]
    fn summary_line_skips_missing_fields_and_falls_back() {
        let catalog = fx::sample_catalog();
        let provider = catalog.children[0].element.as_ref().unwrap();
        // protocol 缺席 → 只剩 name。
        let doc = SettingsDoc::new(
            json!({"providers": [{"name": "solo"}]}),
            json!({}),
            HashMap::new(),
            "fp".into(),
        );
        assert_eq!(
            summary_line(
                &doc,
                Root::Gateway,
                provider,
                "providers[0]",
                &provider.summary_fields
            ),
            "solo"
        );
        // 全缺席 → 回落第一个标量子字段 → 也没有 → "(空)"。
        let doc = fx::empty_doc();
        assert_eq!(
            summary_line(
                &doc,
                Root::Gateway,
                provider,
                "providers[0]",
                &provider.summary_fields
            ),
            "(空)"
        );
    }

    #[test]
    fn summary_line_masks_secrets_and_uses_defaults_for_missing_values() {
        let item = fx::element(fx::object(
            "Item",
            vec![
                fx::secret_field("key"),
                fx::enum_field("mode", &[("a", "A")]),
            ],
        ));
        let mut item = fx::with_paths(item, "items");
        let mut mode = item.children[1].clone();
        mode.has_default = true;
        mode.default = Some(json!("a"));
        item.children[1] = mode;
        let fields = vec!["key".to_string(), "mode".to_string()];
        let doc = SettingsDoc::new(
            json!({"items": [{"key": "sk-abcdefgh"}]}),
            json!({}),
            HashMap::new(),
            "fp".into(),
        );
        assert_eq!(
            summary_line(&doc, Root::Gateway, &item, "items[0]", &fields),
            "•••• · a",
            "密文不进摘要；缺席字段回落声明默认"
        );
    }

    // ── 具体路径解析 ─────────────────────────────────────────

    #[test]
    fn concrete_node_resolves_paths_including_variant_lists() {
        let catalog = fx::sample_catalog();
        let mut doc = fx::sample_doc();
        doc.set_value(Root::Gateway, "providers[0].models[1]", json!({"id": "x"}));
        assert_eq!(
            concrete_node(&catalog, &doc, Root::Gateway, "providers[0].name")
                .unwrap()
                .key,
            "name"
        );
        assert_eq!(
            concrete_node(&catalog, &doc, Root::Gateway, "providers[0].models[0]")
                .unwrap()
                .kind,
            SettingKind::Str,
            "裸字符串挑 str 形态"
        );
        assert_eq!(
            concrete_node(&catalog, &doc, Root::Gateway, "providers[0].models[1]")
                .unwrap()
                .display_label(),
            "ModelSpec",
            "对象挑 ModelSpec 形态"
        );
        assert!(
            concrete_node(&catalog, &doc, Root::Gateway, "providers[9].name").is_some(),
            "下标不查界"
        );
        assert!(concrete_node(&catalog, &doc, Root::Gateway, "nope").is_none());
    }

    #[test]
    fn render_scalar_formats_numbers_booleans_and_maps() {
        assert_eq!(render_scalar(&json!(42)), "42");
        assert_eq!(render_scalar(&json!(1.5)), "1.5");
        assert_eq!(render_scalar(&json!(300.0)), "300.0");
        assert_eq!(render_scalar(&json!(true)), "enabled");
        assert_eq!(render_scalar(&json!(false)), "disabled");
        assert_eq!(render_scalar(&json!("中文")), "中文");
        assert_eq!(render_scalar(&json!({"a": 1})), "{\"a\":1}");
        assert_eq!(render_scalar(&json!(null)), "(null)");
    }
}
