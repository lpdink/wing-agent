//! 列表全结构编辑：定位目标列表、新增（含 stub 与变体形态）、删除、上下移。
//!
//! 文档本身怎么改（路径行走 / 下标 / 形状）是 [`crate::shared::doc_edit`] 的唯一实现，
//! 这里只做面板这一侧：目录定位、`AddOutcome` 的编排（展开谁、光标落哪、开不开编辑器），
//! 以及把「没做成」读成**无操作**（面板没有报错出口）。
//!
//! 索引重排不在这里 —— 删除 / 移动之后要重排 `dirty` / `expanded` / 选择项里的下标，
//! 那是 [`super::SettingsPanel`] 的编排（用 `doc_edit::remap_index`），因为集合不在本模块手里。
//!
//! **已知限制（design A2）**：协议不携带列表的声明默认值（`object`/`list` 的 `default` 为
//! `null`），所以「键缺席 = 跟随默认」的列表在面板里显示为空；一旦新增一项，这份列表就被
//! 物化并钉住（稀疏文档模型的固有语义）。

use wing_api_client::models::PathStep;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;
use wing_api_client::models::parse_path;

use super::doc::Root;
use super::doc::SettingsDoc;
use super::tree::concrete_node;
use crate::shared::doc_edit;
use crate::shared::doc_edit::Policy;

/// 从某个具体路径向上找**最近的列表祖先**（含自己，当自己是列表行时）。
///
/// 列表行 / `(+ 新增一项)` 行（路径 `<列表>[]`）/ 列表项行 / 项内任意字段行都能定位；
/// 根头行等没有列表祖先的路径返回 `None`。`variants` 列表在元素处停下（形态未知，
/// 见 design A14）—— 那正是我们要的「最近的列表」。
pub(crate) fn nearest_list(catalog: &SettingNode, path: &str) -> Option<String> {
    let steps = parse_path(path)?;
    let mut node = catalog;
    let mut concrete = String::new();
    let mut last_list: Option<String> = None;
    for step in &steps {
        match step {
            PathStep::Key(name) => {
                let Some(child) = node.children.iter().find(|child| child.key == *name) else {
                    break;
                };
                concrete = doc_edit::join_path(&concrete, name);
                node = child;
            }
            PathStep::Index(index) => {
                let Some(element) = node.element.as_deref() else {
                    break;
                };
                concrete = doc_edit::index_path(&concrete, *index);
                node = element;
            }
            PathStep::Element => {
                let Some(element) = node.element.as_deref() else {
                    break;
                };
                concrete.push_str("[]");
                node = element;
            }
        }
        if node.kind == SettingKind::List {
            last_list = Some(concrete.clone());
        }
    }
    last_list
}

/// 这一行是不是某个列表的**项**：返回（列表路径, 下标）。
///
/// 判定靠路径末段是 `[i]` 且其父路径在目录里确实是 `list`（这样普通对象的字段行
/// 不会被误判）。
pub(crate) fn list_item_parent(
    catalog: &SettingNode,
    doc: &SettingsDoc,
    root: Root,
    path: &str,
) -> Option<(String, usize)> {
    let steps = parse_path(path)?;
    let (last, prefix) = steps.split_last()?;
    let PathStep::Index(index) = last else {
        return None;
    };
    let list_path = doc_edit::format_path(prefix);
    let list_node = concrete_node(catalog, doc, root, &list_path)?;
    (list_node.kind == SettingKind::List).then_some((list_path, *index))
}

/// 新增一项的结果：面板据此展开、移光标、开编辑器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddOutcome {
    /// 新项的下标。
    pub index: usize,
    /// 新项的具体路径。
    pub item_path: String,
    /// 需要立即打开编辑器的路径（标量项）。
    pub edit_at: Option<String>,
    /// 光标应该落在哪一行（对象项 = 第一个必填字段，标量项 = 自己）。
    pub cursor_at: String,
}

/// 追加一项（`models` 这类 union 列表需要先选形态 → `variant` 是形态下标）。
pub(crate) fn add_item(
    doc: &mut SettingsDoc,
    root: Root,
    list_path: &str,
    list_node: &SettingNode,
    variant: Option<usize>,
) -> Option<AddOutcome> {
    let element = match (
        list_node.element.as_deref(),
        list_node.variants.as_deref(),
        variant,
    ) {
        (Some(element), _, _) => element,
        (None, Some(variants), Some(index)) => variants.get(index)?,
        _ => return None,
    };
    let steps = parse_path(list_path)?;
    let index = doc_edit::append_item(
        doc.root_doc_mut(root),
        &steps,
        list_node,
        doc_edit::empty_value(element, Policy::Lenient)?,
        Policy::Lenient,
    )
    .ok()?;
    let item_path = doc_edit::index_path(list_path, index);
    let cursor_at = if element.kind == SettingKind::Object || element.kind == SettingKind::List {
        first_required_path(element, &item_path).unwrap_or_else(|| item_path.clone())
    } else {
        item_path.clone()
    };
    let edit_at = matches!(
        element.kind,
        SettingKind::Str
            | SettingKind::Secret
            | SettingKind::Int
            | SettingKind::Float
            | SettingKind::Map
    )
    .then(|| item_path.clone());
    Some(AddOutcome {
        index,
        item_path,
        edit_at,
        cursor_at,
    })
}

/// 删除一个项；下标越界或不是数组 → `false`（面板策略：没做成 = 无操作）。
pub(crate) fn remove_item(
    doc: &mut SettingsDoc,
    root: Root,
    list_path: &str,
    index: usize,
) -> bool {
    let Some(steps) = parse_path(&doc_edit::index_path(list_path, index)) else {
        return false;
    };
    doc_edit::remove_indexed(doc.root_doc_mut(root), &steps).is_ok()
}

/// 把一个项移动 `delta` 格（±1）；到了边界返回 `None`。
pub(crate) fn move_item(
    doc: &mut SettingsDoc,
    root: Root,
    list_path: &str,
    index: usize,
    delta: isize,
) -> Option<usize> {
    let steps = parse_path(&doc_edit::index_path(list_path, index))?;
    // 面板策略（`Policy::Lenient`）：越界 = 不移动；形状问题读成无操作（`.ok()?`）。
    let outcome = doc_edit::move_indexed(
        doc.root_doc_mut(root),
        &steps,
        delta as i64,
        Policy::Lenient,
    )
    .ok()?;
    outcome.moved.then_some(outcome.to)
}

/// 第一个必填子字段的路径（向导手感：新增后光标落在这里）。
fn first_required_path(node: &SettingNode, item_path: &str) -> Option<String> {
    node.children
        .iter()
        .find(|child| child.required)
        .map(|child| doc_edit::join_path(item_path, &child.key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::test_support as fx;
    use serde_json::json;

    #[test]
    fn nearest_list_walks_up_from_fields_items_and_add_rows() {
        let catalog = fx::sample_catalog();
        assert_eq!(
            nearest_list(&catalog, "providers[0].models[1].id").as_deref(),
            Some("providers[0].models"),
            "最近的列表祖先"
        );
        assert_eq!(
            nearest_list(&catalog, "providers[0].name").as_deref(),
            Some("providers")
        );
        assert_eq!(
            nearest_list(&catalog, "providers").as_deref(),
            Some("providers"),
            "列表行自己"
        );
        assert_eq!(
            nearest_list(&catalog, "providers[]").as_deref(),
            Some("providers"),
            "`(+ 新增一项)` 行"
        );
        assert_eq!(
            nearest_list(&catalog, "providers[0].models[]").as_deref(),
            Some("providers[0].models")
        );
        assert_eq!(
            nearest_list(&catalog, "").as_deref(),
            None,
            "根头没有列表祖先"
        );
        assert_eq!(nearest_list(&catalog, "gateway.port").as_deref(), None);
        assert_eq!(
            nearest_list(&catalog, "providers[0].models[0]").as_deref(),
            Some("providers[0].models"),
            "union 元素处停下"
        );
    }

    #[test]
    fn list_item_parent_only_accepts_actual_list_items() {
        let catalog = fx::sample_catalog();
        let doc = fx::sample_doc();
        assert_eq!(
            list_item_parent(&catalog, &doc, Root::Gateway, "providers[0]"),
            Some(("providers".into(), 0))
        );
        assert_eq!(
            list_item_parent(&catalog, &doc, Root::Gateway, "tools[1]"),
            Some(("tools".into(), 1))
        );
        assert_eq!(
            list_item_parent(&catalog, &doc, Root::Gateway, "providers[0].models[0]"),
            Some(("providers[0].models".into(), 0))
        );
        assert_eq!(
            list_item_parent(&catalog, &doc, Root::Gateway, "providers"),
            None
        );
        assert_eq!(
            list_item_parent(&catalog, &doc, Root::Gateway, "gateway.port"),
            None
        );
        assert_eq!(
            list_item_parent(&catalog, &doc, Root::Gateway, "providers[]"),
            None,
            "新增行不是项"
        );
    }

    #[test]
    fn a_malformed_list_path_is_not_an_add_target() {
        let catalog = fx::sample_catalog();
        let mut doc = fx::sample_doc();
        let tools = &catalog.children[2];
        assert!(add_item(&mut doc, Root::Gateway, "tools[", tools, None).is_none());
        assert_eq!(
            doc.value(Root::Gateway, "tools"),
            Some(&json!(["Bash", "Read"])),
            "文档一个字节都没动"
        );
    }

    #[test]
    fn adding_a_scalar_item_appends_an_empty_value_and_opens_the_editor() {
        let catalog = fx::sample_catalog();
        let mut doc = fx::sample_doc();
        let tools = &catalog.children[2];
        let outcome = add_item(&mut doc, Root::Gateway, "tools", tools, None).unwrap();
        assert_eq!(outcome.index, 2);
        assert_eq!(outcome.item_path, "tools[2]");
        assert_eq!(
            outcome.edit_at.as_deref(),
            Some("tools[2]"),
            "标量项立即编辑"
        );
        assert_eq!(outcome.cursor_at, "tools[2]");
        assert_eq!(doc.value(Root::Gateway, "tools[2]"), Some(&json!("")));
    }

    #[test]
    fn adding_an_object_item_writes_a_stub_with_required_fields_only() {
        let catalog = fx::sample_catalog();
        let mut doc = fx::sample_doc();
        let providers = &catalog.children[0];
        let outcome = add_item(&mut doc, Root::Gateway, "providers", providers, None).unwrap();
        assert_eq!(outcome.index, 1);
        assert_eq!(outcome.edit_at, None, "对象项不直接开编辑器");
        assert_eq!(
            outcome.cursor_at, "providers[1].name",
            "光标落在第一个必填字段"
        );
        assert_eq!(
            doc.value(Root::Gateway, "providers[1]"),
            Some(&json!({"name": "", "base_url": ""})),
            "只写必填；protocol / api_key / models 缺席（跟随默认）"
        );
    }

    #[test]
    fn adding_to_an_empty_list_creates_the_array() {
        let catalog = fx::sample_catalog();
        let mut doc = fx::empty_doc();
        let providers = &catalog.children[0];
        let outcome = add_item(&mut doc, Root::Gateway, "providers", providers, None).unwrap();
        assert_eq!(outcome.index, 0);
        assert_eq!(
            doc.value(Root::Gateway, "providers[0].name"),
            Some(&json!(""))
        );
    }

    #[test]
    fn adding_to_a_union_list_requires_a_variant_and_respects_it() {
        let catalog = fx::sample_catalog();
        let mut doc = fx::sample_doc();
        let models = catalog.children[0]
            .element
            .as_ref()
            .unwrap()
            .children
            .iter()
            .find(|child| child.key == "models")
            .unwrap();
        // 没有 variant 指标 → 拒绝。
        assert!(add_item(&mut doc, Root::Gateway, "providers[0].models", models, None).is_none());
        // 简单形态（str）。
        let outcome = add_item(
            &mut doc,
            Root::Gateway,
            "providers[0].models",
            models,
            Some(0),
        )
        .unwrap();
        assert_eq!(outcome.item_path, "providers[0].models[1]");
        assert_eq!(outcome.edit_at.as_deref(), Some("providers[0].models[1]"));
        assert_eq!(
            doc.value(Root::Gateway, "providers[0].models[1]"),
            Some(&json!(""))
        );
        // 完整形态（ModelSpec）。
        let outcome = add_item(
            &mut doc,
            Root::Gateway,
            "providers[0].models",
            models,
            Some(1),
        )
        .unwrap();
        assert_eq!(
            doc.value(Root::Gateway, "providers[0].models[2]"),
            Some(&json!({})),
            "ModelSpec 没有必填字段 → 空对象"
        );
        assert_eq!(outcome.edit_at, None);
        // 越界的形态下标 → 拒绝。
        assert!(
            add_item(
                &mut doc,
                Root::Gateway,
                "providers[0].models",
                models,
                Some(9)
            )
            .is_none()
        );
    }

    #[test]
    fn removing_shifts_the_remaining_items() {
        let mut doc = fx::sample_doc();
        assert!(remove_item(&mut doc, Root::Gateway, "tools", 0));
        assert_eq!(doc.value(Root::Gateway, "tools"), Some(&json!(["Read"])));
        assert!(!remove_item(&mut doc, Root::Gateway, "tools", 5), "越界");
        assert!(!remove_item(&mut doc, Root::Gateway, "nope", 0), "不是数组");
    }

    #[test]
    fn moving_swaps_items_and_respects_bounds() {
        let mut doc = fx::sample_doc();
        assert_eq!(move_item(&mut doc, Root::Gateway, "tools", 0, 1), Some(1));
        assert_eq!(
            doc.value(Root::Gateway, "tools"),
            Some(&json!(["Read", "Bash"]))
        );
        assert_eq!(
            move_item(&mut doc, Root::Gateway, "tools", 1, 1),
            None,
            "到底了"
        );
        assert_eq!(
            move_item(&mut doc, Root::Gateway, "tools", 0, -1),
            None,
            "到顶了"
        );
        assert_eq!(
            move_item(&mut doc, Root::Gateway, "tools", 2, -1),
            None,
            "下标等于长度（行已过期）也不是可移动的项"
        );
        assert_eq!(move_item(&mut doc, Root::Gateway, "nope", 0, 1), None);
    }

    #[test]
    fn stubs_recurse_into_required_objects_and_lists() {
        let inner = fx::object("inner", vec![fx::required_str("deep")]);
        let outer = fx::object(
            "outer",
            vec![
                fx::required_str("name"),
                {
                    let mut n = inner;
                    n.required = true;
                    n
                },
                {
                    let mut n = fx::list("tags", fx::element(fx::str_field("t")));
                    n.required = true;
                    n
                },
            ],
        );
        assert_eq!(
            doc_edit::empty_value(&outer, Policy::Lenient),
            Some(json!({"name": "", "inner": {"deep": ""}, "tags": []}))
        );
    }
}
