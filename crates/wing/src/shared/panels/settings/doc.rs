//! 文档模型：两个根的稀疏文档 + 密文表 + 脏标记。
//!
//! **稀疏文档**（design.md §7.1 / D14）：只有用户显式写下的键，缺席 = 跟随声明默认值。
//! 面板持有的两份文档（Gateway 的 config.yaml 与 Interface 的 TUI 配置）形状完全相同，
//! 用 [`Root`] 区分；**服务端不缓存**，每次打开 / 重载都是一次快照。
//!
//! 这个模块只装事实（值 / baseline / dirty / 密文 / 指纹），不装策略：问题、行、编辑器都在
//! 别的模块。`dirty` 是**按具体路径累积**的集合（改动即脏，见 design D8），列表增删移之后
//! 必须用 [`crate::shared::doc_edit::remap_index`] 把下标重排 —— 否则脏标记会漂到别的项上
//! （design D6）。
//!
//! 路径代数与文档编辑原语是 [`crate::shared::doc_edit`] 的**唯一实现**（`wing config` 与
//! 面板共用）；这里只把它包成「按根寻址 + 脏标记 + 密文」的这一层，并固定
//! [`Policy::Lenient`]（面板的 UX 语义：按需创建容器、清理空容器残渣，从不报错）。

use std::collections::HashMap;
use std::collections::HashSet;

use serde_json::Map;
use serde_json::Value;
use wing_api_client::models::SecretState;
use wing_api_client::models::parse_path;

use crate::shared::doc_edit;
use crate::shared::doc_edit::Policy;

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

    /// 根文档的可变句柄（面板的编辑原语都在 [`crate::shared::doc_edit`] 里，
    /// 它们收 `&mut Value`）。
    pub(crate) fn root_doc_mut(&mut self, root: Root) -> &mut Value {
        match root {
            Root::Gateway => &mut self.gateway,
            Root::Interface => &mut self.interface,
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
        let steps = parse_path(path)?;
        doc_edit::get_path(self.root_doc(root), &steps)
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

    /// 写一个具体路径的值（中间容器按需创建）。面板策略（[`Policy::Lenient`]）不会失败。
    pub(crate) fn set_value(&mut self, root: Root, path: &str, value: Value) {
        let Some(steps) = parse_path(path) else {
            return;
        };
        // `let _`：宽容策略把形状问题就地修掉，没有错误出口。
        let _ = doc_edit::set_path(self.root_doc_mut(root), &steps, value, Policy::Lenient);
    }

    /// 从稀疏文档移除一个路径（「回到跟随默认」）；路径本来就缺席 → `false`。
    /// 逐级清理因此变空的 object / 数组（不让 `{"gateway":{}}` 这样的残渣留在文件里）。
    pub(crate) fn remove_value(&mut self, root: Root, path: &str) -> bool {
        let Some(steps) = parse_path(path) else {
            return false;
        };
        doc_edit::unset_path(self.root_doc_mut(root), &steps, Policy::Lenient).unwrap_or(false)
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
            .any(|(r, p)| *r == root && doc_edit::path_is_within(path, p))
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
            if let Some(mapped) = doc_edit::remap_index(path, list_path, map) {
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
    fn remap_dirty_rewrites_and_drops_entries_of_one_root_only() {
        let mut doc = sample();
        doc.mark_dirty(Root::Gateway, "providers[0].api_key");
        doc.mark_dirty(Root::Gateway, "providers[2].name");
        doc.mark_dirty(Root::Gateway, "agents[2].model");
        doc.mark_dirty(Root::Interface, "colors.accent");
        doc.remap_dirty(Root::Gateway, "providers", &doc_edit::remove_index_map(1));
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
        doc2.remap_dirty(Root::Gateway, "providers", &doc_edit::remove_index_map(1));
        assert_eq!(doc2.dirty_count_in(Root::Gateway), 1);
        assert!(
            doc2.is_dirty(Root::Gateway, "providers[1].name"),
            "原来的 [2] 补到 [1]"
        );
        assert!(!doc2.is_dirty(Root::Gateway, "providers[2].name"));
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
