//! 文档模型：两个根的稀疏文档 + 密文表 + 脏标记 + 路径代数。
//!
//! **稀疏文档**（design.md §7.1 / D14）：只有用户显式写下的键，缺席 = 跟随声明默认值。
//! 面板持有的两份文档（Gateway 的 config.yaml 与 Interface 的 TUI 配置）形状完全相同，
//! 用 [`Root`] 区分；**服务端不缓存**，每次打开 / 重载都是一次快照。
//!
//! 这个模块只装事实（值 / baseline / dirty / 密文 / 指纹），不装策略：问题、行、编辑器都在
//! 别的模块。`dirty` 是**按具体路径累积**的集合（改动即脏，见 design D8），列表增删移之后
//! 必须用 [`remap_index`] 把下标重排 —— 否则脏标记会漂到别的项上（design D6）。

use std::collections::HashMap;
use std::collections::HashSet;

use serde_json::Map;
use serde_json::Value;
use wing_api_client::models::PathStep;
use wing_api_client::models::SecretState;
use wing_api_client::models::parse_path;

/// 面板的两个顶层根（design.md §11.4）。
///
/// 一棵树、两个根：Gateway 是后端 `config.yaml`（catalog 来自 `GET /api/settings/schema`），
/// Interface 是 TUI 自己的 `~/.wing/tui/config.yaml`（catalog 由 Rust 侧声明，09 步骤提供）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Root {
    Gateway,
    Interface,
}

impl Root {
    /// 根头行的显示名（08 也可用它做 Tab / 键位栏文案）。
    pub fn label(self) -> &'static str {
        match self {
            Root::Gateway => "Gateway",
            Root::Interface => "Interface",
        }
    }
}

/// 稀疏文档 + 打开时的快照（baseline）+ 脏标记 + 密文表 + 指纹。
///
/// 值的寻址一律走 §5.2 的规范路径（具体下标，`providers[0].api_key`）；
/// [`Self::value`] 只读，[`Self::set_value`] / [`Self::remove_value`] 是面板唯一的写入口。
#[derive(Debug, Clone)]
pub struct SettingsDoc {
    gateway: Value,
    interface: Value,
    baseline_gateway: Value,
    baseline_interface: Value,
    dirty: HashSet<(Root, String)>,
    secrets: HashMap<String, SecretState>,
    fingerprint: String,
}

impl SettingsDoc {
    /// 从 `get` 响应（或 09 的 `read_interface_doc`）建立文档：当前值同时成为 baseline
    /// （打开面板这一刻就是「未改动」的参照）。
    ///
    /// 非 object 的顶层值一律规范化为 `{}`（协议保证是 object；这里是防御，免得后续
    /// 每处取值都要再判一次形状）。
    pub fn new(
        gateway: Value,
        interface: Value,
        secrets: HashMap<String, SecretState>,
        fingerprint: String,
    ) -> Self {
        let gateway = as_object(gateway);
        let interface = as_object(interface);
        Self {
            baseline_gateway: gateway.clone(),
            baseline_interface: interface.clone(),
            gateway,
            interface,
            dirty: HashSet::new(),
            secrets,
            fingerprint,
        }
    }

    // ── 读 ────────────────────────────────────────────────────

    /// 根文档的整份值（`Save` / 预览动作携带它）。
    pub fn root_doc(&self, root: Root) -> &Value {
        match root {
            Root::Gateway => &self.gateway,
            Root::Interface => &self.interface,
        }
    }

    /// 打开面板时（或上次保存后）的文档快照。
    pub fn baseline(&self, root: Root) -> &Value {
        match root {
            Root::Gateway => &self.baseline_gateway,
            Root::Interface => &self.baseline_interface,
        }
    }

    /// 按规范路径取值；路径上的任一段缺席即 `None`（下标越界同样 `None`——不查界、
    /// 见 P3 的裁定）。
    pub fn value(&self, root: Root, path: &str) -> Option<&Value> {
        get_path(self.root_doc(root), path)
    }

    /// 路径是否在稀疏文档里显式存在。
    pub fn contains(&self, root: Root, path: &str) -> bool {
        self.value(root, path).is_some()
    }

    /// 密文状态（只对 Gateway 根有意义：`get` 发下来的 `secrets` 表）。
    pub fn secret_state(&self, root: Root, path: &str) -> Option<&SecretState> {
        match root {
            Root::Gateway => self.secrets.get(path),
            // Interface 的密文来自本地文件（真值就在文档里），没有三态表。
            Root::Interface => None,
        }
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    // ── 写（只给面板用） ──────────────────────────────────────

    /// 写一个具体路径的值（中间容器按需创建）。
    pub(crate) fn set_value(&mut self, root: Root, path: &str, value: Value) {
        let doc = match root {
            Root::Gateway => &mut self.gateway,
            Root::Interface => &mut self.interface,
        };
        set_path(doc, path, value);
    }

    /// 从稀疏文档移除一个路径（「回到跟随默认」）；路径本来就缺席 → `false`。
    /// 逐级清理因此变空的 object（不让 `{"gateway":{}}` 这样的残渣留在文件里）。
    pub(crate) fn remove_value(&mut self, root: Root, path: &str) -> bool {
        let doc = match root {
            Root::Gateway => &mut self.gateway,
            Root::Interface => &mut self.interface,
        };
        remove_path(doc, path)
    }

    /// 把当前文档记为新的 baseline 并清空该根的脏标记（保存成功 / 注入 Interface 后调用）。
    pub(crate) fn mark_baseline(&mut self, root: Root) {
        match root {
            Root::Gateway => self.baseline_gateway = self.gateway.clone(),
            Root::Interface => self.baseline_interface = self.interface.clone(),
        }
        self.dirty.retain(|(r, _)| *r != root);
    }

    /// `R` 重载：整份换掉 Gateway 文档 + 密文表 + 指纹（新的当前值即新的 baseline）。
    pub(crate) fn reload_gateway(
        &mut self,
        gateway: Value,
        secrets: HashMap<String, SecretState>,
        fingerprint: String,
    ) {
        self.gateway = as_object(gateway);
        self.baseline_gateway = self.gateway.clone();
        self.secrets = secrets;
        self.fingerprint = fingerprint;
        self.dirty.retain(|(r, _)| *r != Root::Gateway);
    }

    /// 注入 / 刷新 Interface 文档（同时更新 baseline；Interface 无乐观并发指纹）。
    pub(crate) fn set_interface_doc(&mut self, interface: Value) {
        self.interface = as_object(interface);
        self.baseline_interface = self.interface.clone();
        self.dirty.retain(|(r, _)| *r != Root::Interface);
    }

    pub(crate) fn set_fingerprint(&mut self, fingerprint: String) {
        self.fingerprint = fingerprint;
    }

    // ── 脏标记 ────────────────────────────────────────────────

    /// 改动即脏（不因「改回原值」自动清除，见 design D8）。
    pub(crate) fn mark_dirty(&mut self, root: Root, path: &str) {
        self.dirty.insert((root, path.to_string()));
    }

    /// 该 **具体路径** 自己是否被标脏（不含子孙）。
    pub fn is_dirty(&self, root: Root, path: &str) -> bool {
        self.dirty.contains(&(root, path.to_string()))
    }

    /// 该行（可能是结构节点）自己或**子孙**是否被标脏 —— 行标记的汇总口径。
    pub(crate) fn has_dirty_below(&self, root: Root, path: &str) -> bool {
        self.dirty
            .iter()
            .any(|(r, p)| *r == root && path_is_within(path, p))
    }

    /// 某个根的脏路径数。
    pub(crate) fn dirty_count_in(&self, root: Root) -> usize {
        self.dirty.iter().filter(|(r, _)| *r == root).count()
    }

    /// 两个根的脏路径总数（标题栏 `N unsaved`）。
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// 重排某个根里、落在 `list_path` 之下的脏路径下标（design D6）。
    pub(crate) fn remap_dirty(
        &mut self,
        root: Root,
        list_path: &str,
        map: &dyn Fn(usize) -> Option<usize>,
    ) {
        let mut next = HashSet::with_capacity(self.dirty.len());
        for (r, path) in &self.dirty {
            if *r != root {
                next.insert((*r, path.clone()));
                continue;
            }
            if let Some(mapped) = remap_index(path, list_path, map) {
                next.insert((*r, mapped));
            }
        }
        self.dirty = next;
    }
}

/// 非 object → `{}`（顶层文档的形状防御）。
fn as_object(value: Value) -> Value {
    match value {
        Value::Object(_) => value,
        _ => Value::Object(Map::new()),
    }
}

// ── 路径代数 ─────────────────────────────────────────────────
//
// 路径文法在 design.md §5.2 冻结，解析器是 06 的 [`parse_path`]（三变体 Key/Index/Element，
// 下标不查界）。这个模块只加「用」它的原语：取值 / 写值 / 删值 / 拼接 / 前缀重排。

/// 取规范路径上的值（对象按 key、数组按下标）。
pub(crate) fn get_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let steps = parse_path(path)?;
    let mut value = doc;
    for step in &steps {
        value = match step {
            PathStep::Key(name) => value.get(name)?,
            PathStep::Index(i) => value.get(*i)?,
            // 模板段（`[]`）只出现在 catalog 路径里；文档路径用它 = 取不到。
            PathStep::Element => return None,
        };
    }
    Some(value)
}

/// 写规范路径上的值；中间容器按需创建（缺 object / array 时新建）。
pub(crate) fn set_path(doc: &mut Value, path: &str, new_value: Value) {
    let Some(steps) = parse_path(path) else {
        return;
    };
    let mut cursor = doc;
    for step in &steps {
        match step {
            PathStep::Key(name) => {
                if !cursor.is_object() {
                    *cursor = Value::Object(Map::new());
                }
                cursor = cursor
                    .as_object_mut()
                    .expect("just normalized")
                    .entry(name.clone())
                    .or_insert(Value::Null);
            }
            PathStep::Index(i) => {
                if !cursor.is_array() {
                    *cursor = Value::Array(Vec::new());
                }
                let array = cursor.as_array_mut().expect("just normalized");
                while array.len() <= *i {
                    array.push(Value::Null);
                }
                cursor = &mut array[*i];
            }
            PathStep::Element => return,
        }
    }
    *cursor = new_value;
}

/// 移除规范路径上的值；顺带清理由此变空的 object 祖先。
/// 路径不存在（或中间形状不符）→ `false`。
pub(crate) fn remove_path(doc: &mut Value, path: &str) -> bool {
    let Some(steps) = parse_path(path) else {
        return false;
    };
    fn walk(value: &mut Value, steps: &[PathStep]) -> bool {
        let Some((first, rest)) = steps.split_first() else {
            return false;
        };
        match first {
            PathStep::Key(name) => {
                let Some(object) = value.as_object_mut() else {
                    return false;
                };
                if rest.is_empty() {
                    return object.remove(name).is_some();
                }
                let removed = object.get_mut(name).is_some_and(|child| walk(child, rest));
                if removed && object.get(name).is_some_and(is_empty_container) {
                    object.remove(name);
                }
                removed
            }
            PathStep::Index(i) => {
                let Some(array) = value.as_array_mut() else {
                    return false;
                };
                if rest.is_empty() {
                    if *i < array.len() {
                        array.remove(*i);
                        return true;
                    }
                    return false;
                }
                array.get_mut(*i).is_some_and(|child| walk(child, rest))
            }
            PathStep::Element => false,
        }
    }
    walk(doc, &steps)
}

/// 空 object / 空数组（清理残渣时的判定）。
fn is_empty_container(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.is_empty(),
        Value::Array(array) => array.is_empty(),
        _ => false,
    }
}

/// 把子段拼到父路径上（空父路径 → 子段本身）。
pub(crate) fn join_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        segment.to_string()
    } else {
        format!("{parent}.{segment}")
    }
}

/// 数组下标拼到列表路径上：`providers` + 0 → `providers[0]`。
pub(crate) fn index_path(list_path: &str, index: usize) -> String {
    format!("{list_path}[{index}]")
}

/// 用步骤重建规范路径（`Key` 后紧跟的 `Index`/`Element` 不带点）。
pub(crate) fn format_path(steps: &[PathStep]) -> String {
    let mut out = String::new();
    for step in steps {
        match step {
            PathStep::Key(name) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(name);
            }
            PathStep::Index(i) => {
                out.push('[');
                out.push_str(&i.to_string());
                out.push(']');
            }
            PathStep::Element => out.push_str("[]"),
        }
    }
    out
}

/// 某个具体路径的**全部祖先前缀**（不含自己，含根的空路径）——
/// `providers[0].api_key` → `["", "providers", "providers[0]"]`。
pub(crate) fn ancestors(path: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let Some(steps) = parse_path(path) else {
        return out;
    };
    for i in 1..steps.len() {
        out.push(format_path(&steps[..i]));
    }
    out
}

/// `outer` 是不是 `inner` 的**段边界**前缀（`providers` 匹配 `providers[0].x`，
/// 不匹配 `providersx`）。
pub(crate) fn path_is_within(outer: &str, inner: &str) -> bool {
    if outer.is_empty() {
        return true;
    }
    match inner.strip_prefix(outer) {
        Some(rest) => rest.is_empty() || rest.starts_with('.') || rest.starts_with('['),
        None => false,
    }
}

/// 索引重排（design D6）：`path` 的步骤前缀恰为 `list_path` 时，把**紧随其后的那个
/// `[i]`** 按下标映射换成新值；映射返回 `None` = 该项已消失（整条路径丢弃）。
/// 前缀不匹配 / 后面不是下标 → 原样返回。更深的层级不动。
pub(crate) fn remap_index(
    path: &str,
    list_path: &str,
    map: &dyn Fn(usize) -> Option<usize>,
) -> Option<String> {
    let Some(mut steps) = parse_path(path) else {
        return Some(path.to_string());
    };
    let Some(prefix) = parse_path(list_path) else {
        return Some(path.to_string());
    };
    if steps.len() <= prefix.len() || steps[..prefix.len()] != prefix[..] {
        return Some(path.to_string());
    }
    let PathStep::Index(index) = steps[prefix.len()] else {
        return Some(path.to_string());
    };
    let new_index = map(index)?;
    steps[prefix.len()] = PathStep::Index(new_index);
    Some(format_path(&steps))
}

/// 删除下标 `removed` 之后的下标映射：等于 `removed` 的丢弃，大于的减一。
pub(crate) fn remove_index_map(removed: usize) -> impl Fn(usize) -> Option<usize> {
    move |i| match i.cmp(&removed) {
        std::cmp::Ordering::Less => Some(i),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(i - 1),
    }
}

/// 交换两个下标的映射（列表项上移 / 下移）。
pub(crate) fn swap_index_map(a: usize, b: usize) -> impl Fn(usize) -> Option<usize> {
    move |i| {
        if i == a {
            Some(b)
        } else if i == b {
            Some(a)
        } else {
            Some(i)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> SettingsDoc {
        SettingsDoc::new(
            json!({
                "providers": [
                    {"name": "default", "api_key": null},
                    {"name": "second"}
                ],
                "gateway": {"port": 32523}
            }),
            json!({"colors": {"accent": "cyan"}}),
            HashMap::new(),
            "fp-1".into(),
        )
    }

    // ── 路径取值 / 写值 / 删值 ────────────────────────────────

    #[test]
    fn value_reads_nested_objects_and_indexed_arrays() {
        let doc = sample();
        assert_eq!(
            doc.value(Root::Gateway, "providers[0].name"),
            Some(&json!("default"))
        );
        assert_eq!(
            doc.value(Root::Gateway, "gateway.port"),
            Some(&json!(32523))
        );
        assert_eq!(
            doc.value(Root::Gateway, ""),
            Some(doc.root_doc(Root::Gateway))
        );
        assert_eq!(
            doc.value(Root::Interface, "colors.accent"),
            Some(&json!("cyan"))
        );
    }

    #[test]
    fn value_missing_paths_are_none_including_out_of_range_indices() {
        let doc = sample();
        assert_eq!(doc.value(Root::Gateway, "nope"), None);
        assert_eq!(
            doc.value(Root::Gateway, "providers[7].name"),
            None,
            "下标不查界但取不到就是 None"
        );
        assert_eq!(
            doc.value(Root::Interface, "providers[0]"),
            None,
            "根之间不串门"
        );
        assert_eq!(
            doc.value(Root::Gateway, "providers[0][]"),
            None,
            "模板段不是文档路径"
        );
        assert_eq!(doc.value(Root::Gateway, "providers[0"), None, "文法非法");
    }

    #[test]
    fn set_value_creates_missing_containers() {
        let mut doc = sample();
        doc.set_value(
            Root::Gateway,
            "providers[2].models[0].id",
            json!("ds-flash"),
        );
        assert_eq!(
            doc.value(Root::Gateway, "providers[2].models[0].id"),
            Some(&json!("ds-flash"))
        );
        // 中间缺位补 null（数组按需加长），不 panic。
        assert_eq!(
            doc.value(Root::Gateway, "providers[2].models[0]"),
            Some(&json!({"id": "ds-flash"}))
        );
    }

    #[test]
    fn removing_a_value_prunes_the_empty_object_residue() {
        let mut doc = sample();
        assert!(doc.remove_value(Root::Gateway, "gateway.port"));
        assert_eq!(
            doc.value(Root::Gateway, "gateway"),
            None,
            "空 object 残渣被清掉"
        );
        assert!(
            !doc.remove_value(Root::Gateway, "gateway.port"),
            "已缺席 → false"
        );
    }

    #[test]
    fn removing_an_array_element_keeps_the_list() {
        let mut doc = sample();
        assert!(doc.remove_value(Root::Gateway, "providers[0]")); // 防御路径：列表删除走 list.rs
        assert_eq!(
            doc.value(Root::Gateway, "providers"),
            Some(&json!([{"name": "second"}]))
        );
    }

    // ── 脏标记 ────────────────────────────────────────────────

    #[test]
    fn dirty_is_per_root_and_accumulates_until_baseline_reset() {
        let mut doc = sample();
        doc.mark_dirty(Root::Gateway, "providers[0].name");
        doc.mark_dirty(Root::Interface, "colors.accent");
        assert!(doc.is_dirty(Root::Gateway, "providers[0].name"));
        assert!(!doc.is_dirty(Root::Gateway, "colors.accent"), "两个根不串");
        assert_eq!(doc.dirty_count(), 2);
        assert_eq!(doc.dirty_count_in(Root::Gateway), 1);
        doc.mark_baseline(Root::Gateway);
        assert_eq!(doc.dirty_count(), 1, "只清该根");
        doc.mark_baseline(Root::Interface);
        assert_eq!(doc.dirty_count(), 0);
    }

    #[test]
    fn dirty_rolls_up_to_ancestors_for_markers() {
        let mut doc = sample();
        doc.mark_dirty(Root::Gateway, "providers[0].name");
        assert!(doc.has_dirty_below(Root::Gateway, "providers"));
        assert!(doc.has_dirty_below(Root::Gateway, ""));
        assert!(!doc.has_dirty_below(Root::Gateway, "providers[1]"));
        assert!(!doc.has_dirty_below(Root::Interface, "providers"), "根隔离");
        // 段边界：providersX 不是 providers 的子孙。
        let mut doc2 = sample();
        doc2.mark_dirty(Root::Gateway, "providersX.name");
        assert!(!doc2.has_dirty_below(Root::Gateway, "providers"));
    }

    // ── 索引重排 ──────────────────────────────────────────────

    #[test]
    fn remap_index_rewrites_only_the_first_index_after_the_list_prefix() {
        let map = remove_index_map(1);
        assert_eq!(
            remap_index("providers[0].api_key", "providers", &map).as_deref(),
            Some("providers[0].api_key")
        );
        assert_eq!(
            remap_index("providers[1].api_key", "providers", &map),
            None,
            "被删项整体丢弃"
        );
        assert_eq!(
            remap_index("providers[2].models[0].id", "providers", &map).as_deref(),
            Some("providers[1].models[0].id"),
            "更深的层级不动"
        );
        assert_eq!(
            remap_index("providers", "providers", &map).as_deref(),
            Some("providers")
        );
        assert_eq!(
            remap_index("agents[1].name", "providers", &map).as_deref(),
            Some("agents[1].name"),
            "别的列表不动"
        );
        assert_eq!(
            remap_index("providers[0].models[2].id", "providers[0].models", &map).as_deref(),
            Some("providers[0].models[1].id"),
            "嵌套列表以自己的路径为前缀"
        );
        assert_eq!(
            remap_index("providers[0].models[1].id", "providers[0].models", &map),
            None,
            "嵌套列表里被删的那一项也整体丢弃"
        );
    }

    #[test]
    fn remap_index_swap_moves_entries_between_items() {
        let map = swap_index_map(1, 2);
        assert_eq!(
            remap_index("providers[1].x", "providers", &map).as_deref(),
            Some("providers[2].x")
        );
        assert_eq!(
            remap_index("providers[2].x", "providers", &map).as_deref(),
            Some("providers[1].x")
        );
        assert_eq!(
            remap_index("providers[0].x", "providers", &map).as_deref(),
            Some("providers[0].x")
        );
        assert_eq!(
            remap_index("agents[1].x", "providers", &map).as_deref(),
            Some("agents[1].x")
        );
    }

    #[test]
    fn remap_dirty_rewrites_and_drops_entries_of_one_root_only() {
        let mut doc = sample();
        doc.mark_dirty(Root::Gateway, "providers[0].api_key");
        doc.mark_dirty(Root::Gateway, "providers[2].name");
        doc.mark_dirty(Root::Gateway, "agents[2].model");
        doc.mark_dirty(Root::Interface, "colors.accent");
        doc.remap_dirty(Root::Gateway, "providers", &remove_index_map(1));
        assert!(doc.is_dirty(Root::Gateway, "providers[0].api_key"));
        assert!(doc.is_dirty(Root::Gateway, "providers[1].name"), "2 → 1");
        assert!(
            doc.is_dirty(Root::Gateway, "agents[2].model"),
            "别的列表不动"
        );
        assert!(doc.is_dirty(Root::Interface, "colors.accent"), "别的根不动");
        // 删除的项自己的脏路径被丢弃，后面的项补位（2 → 1）。
        let mut doc2 = sample();
        doc2.mark_dirty(Root::Gateway, "providers[1].name");
        doc2.mark_dirty(Root::Gateway, "providers[2].name");
        doc2.remap_dirty(Root::Gateway, "providers", &remove_index_map(1));
        assert_eq!(doc2.dirty_count_in(Root::Gateway), 1);
        assert!(
            doc2.is_dirty(Root::Gateway, "providers[1].name"),
            "原来的 [2] 补到 [1]"
        );
        assert!(!doc2.is_dirty(Root::Gateway, "providers[2].name"));
    }

    // ── 路径原语 ──────────────────────────────────────────────

    #[test]
    fn join_and_index_path_build_spec_paths() {
        assert_eq!(join_path("", "providers"), "providers");
        assert_eq!(join_path("providers[0]", "name"), "providers[0].name");
        assert_eq!(index_path("providers", 3), "providers[3]");
        assert_eq!(
            index_path("providers[0].models", 0),
            "providers[0].models[0]"
        );
    }

    #[test]
    fn ancestors_excludes_self_and_starts_at_the_root() {
        assert_eq!(
            ancestors("providers[0].api_key"),
            vec!["", "providers", "providers[0]"]
        );
        assert_eq!(ancestors("gateway"), vec![""]);
        assert_eq!(ancestors(""), vec![""]);
    }

    #[test]
    fn prefix_match_respects_segment_boundaries() {
        assert!(path_is_within("", "anything"));
        assert!(path_is_within("providers", "providers[0].x"));
        assert!(path_is_within("providers[0]", "providers[0].x"));
        assert!(!path_is_within("providers", "providersX"));
        assert!(
            !path_is_within("providers[1]", "providers[10]"),
            "10 不是 1 的子孙"
        );
    }

    #[test]
    fn storage_normalizes_non_object_roots() {
        let doc = SettingsDoc::new(json!([]), Value::Null, HashMap::new(), "fp".into());
        assert_eq!(doc.root_doc(Root::Gateway), &json!({}));
        assert_eq!(doc.root_doc(Root::Interface), &json!({}));
    }

    #[test]
    fn reload_gateway_replaces_values_fingerprint_and_dirty() {
        let mut doc = sample();
        doc.mark_dirty(Root::Gateway, "providers[0].name");
        doc.mark_dirty(Root::Interface, "colors.accent");
        doc.reload_gateway(
            json!({"gateway": {"port": 1}}),
            HashMap::new(),
            "fp-2".into(),
        );
        assert_eq!(doc.value(Root::Gateway, "gateway.port"), Some(&json!(1)));
        assert_eq!(doc.fingerprint(), "fp-2");
        assert_eq!(doc.dirty_count_in(Root::Gateway), 0);
        assert_eq!(doc.dirty_count_in(Root::Interface), 1, "另一个根不受影响");
        assert_eq!(
            doc.baseline(Root::Gateway),
            &json!({"gateway": {"port": 1}})
        );
    }

    #[test]
    fn setting_interface_doc_updates_baseline_and_clears_dirty() {
        let mut doc = sample();
        doc.mark_dirty(Root::Interface, "colors.accent");
        doc.set_interface_doc(json!({"colors": {"accent": "magenta"}}));
        assert_eq!(
            doc.value(Root::Interface, "colors.accent"),
            Some(&json!("magenta"))
        );
        assert_eq!(doc.dirty_count_in(Root::Interface), 0);
        assert_eq!(
            doc.baseline(Root::Interface),
            &json!({"colors": {"accent": "magenta"}})
        );
    }
}
