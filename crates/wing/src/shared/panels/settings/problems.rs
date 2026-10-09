//! 问题清单：本地生成 + 与后端 problems 合并去重 + 严重度排序。
//!
//! 问题有两个来源（design §14.1）：**本地**按 catalog 约束生成（列表长度、必填缺席），
//! **后端**由 `GET /api/settings/get` / `POST set` 带回（`kind` 是字符串，未知种类必须容忍，
//! 见 protocol_addendum P6）。两者按 `(根, 路径, message)` 去重（本地优先 —— 它带 hint）。
//!
//! 本地生成**只覆盖两条约束**：`min_items` / `max_items` 与必填叶子缺席。值级约束
//! （范围 / pattern）不重复校验已存的值：编辑器在提交时挡，后端 `set` 是权威（design D9）。

use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;
use wing_api_client::models::SettingProblem;

use super::doc::Root;
use super::doc::SettingsDoc;
use super::doc::index_path;
use super::doc::join_path;
use super::tree::concrete_node;

/// 面板持有的一条问题（`SettingProblem` + 它属于哪个根）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub root: Root,
    /// 规范路径（具体下标）；`None` = 文档级（无法定位到单个字段）。
    pub path: Option<String>,
    /// 种类字符串（未知种类容忍并排在最后）。
    pub kind: String,
    pub message: String,
    pub hint: Option<String>,
}

/// 本地按 catalog 约束生成的问题（找得到具体路径的那些）。
pub(crate) fn local_problems(doc: &SettingsDoc, roots: &[(Root, &SettingNode)]) -> Vec<Problem> {
    let mut out = Vec::new();
    for (root, catalog) in roots {
        for child in &catalog.children {
            walk(doc, *root, catalog, child, &child.key, &mut out);
        }
    }
    out
}

fn walk(
    doc: &SettingsDoc,
    root: Root,
    catalog: &SettingNode,
    node: &SettingNode,
    path: &str,
    out: &mut Vec<Problem>,
) {
    let readonly = !node.editable || node.apply == wing_api_client::models::ApplyScope::Readonly;
    match node.kind {
        SettingKind::List => {
            let count = item_count(doc, root, path, node);
            if let Some(min) = node.min_items
                && (count as i64) < min
            {
                let (kind, message) = if count == 0 {
                    ("empty_list", format!("不得为空（至少 {min} 项）"))
                } else {
                    (
                        "invalid_value",
                        format!("至少需要 {min} 项（当前 {count} 项）"),
                    )
                };
                out.push(problem(root, Some(path), kind, message));
            }
            if let Some(max) = node.max_items
                && (count as i64) > max
            {
                out.push(problem(
                    root,
                    Some(path),
                    "invalid_value",
                    format!("最多 {max} 项（当前 {count} 项）"),
                ));
            }
            if readonly {
                return;
            }
            for index in 0..count {
                let item_path = index_path(path, index);
                if let Some(item) = concrete_node(catalog, doc, root, &item_path) {
                    walk(doc, root, catalog, item, &item_path, out);
                }
            }
        }
        SettingKind::Object => {
            if readonly {
                return;
            }
            for child in &node.children {
                walk(doc, root, catalog, child, &join_path(path, &child.key), out);
            }
        }
        _ => {
            if readonly {
                return;
            }
            let missing = !doc.contains(root, path);
            if node.required && missing {
                out.push(problem(
                    root,
                    Some(path),
                    "missing_required",
                    "必填项未设置",
                ));
            }
        }
    }
}

fn item_count(doc: &SettingsDoc, root: Root, path: &str, node: &SettingNode) -> usize {
    match doc.value(root, path) {
        Some(serde_json::Value::Array(items)) => items.len(),
        Some(_) => 0,
        None => node
            .default
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len),
    }
}

pub(crate) fn problem(
    root: Root,
    path: Option<&str>,
    kind: &str,
    message: impl Into<String>,
) -> Problem {
    Problem {
        root,
        path: path.map(str::to_string),
        kind: kind.into(),
        message: message.into(),
        hint: None,
    }
}

/// 合并本地与后端问题（**AD4**）：同 `(根, 路径, kind)` 视为同一条 ——
/// `message` 取**后端**的（后端是唯一校验器，文案权威在它），`hint` 取**非空的那个**
/// （后端优先、本地兜底，绝不因为去重把可操作建议丢掉）。合并后按严重度 + 路径排序。
pub(crate) fn merge(local: Vec<Problem>, backend: &[SettingProblem]) -> Vec<Problem> {
    let mut out = local;
    for item in backend {
        let problem = Problem {
            root: Root::Gateway,
            path: item.path.clone(),
            kind: item.kind.clone(),
            message: item.message.clone(),
            hint: item.hint.clone(),
        };
        match out.iter_mut().find(|existing| {
            existing.root == problem.root
                && existing.path == problem.path
                && existing.kind == problem.kind
        }) {
            Some(existing) => {
                existing.message = problem.message;
                if problem.hint.is_some() {
                    existing.hint = problem.hint;
                }
            }
            None => out.push(problem),
        }
    }
    out.sort_by(|a, b| {
        severity_rank(&a.kind)
            .cmp(&severity_rank(&b.kind))
            .then_with(|| match (&a.path, &b.path) {
                (Some(pa), Some(pb)) => pa.cmp(pb),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.message.cmp(&b.message))
    });
    out
}

/// 严重度序（design §14.1；未知种类最后，`conflict` 与未知同档）。
fn severity_rank(kind: &str) -> u8 {
    match kind {
        "missing_required" => 0,
        "unknown_reference" => 1,
        "duplicate" => 2,
        "empty_list" => 3,
        "invalid_value" => 4,
        "unknown_key" => 5,
        _ => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::test_support as fx;
    use serde_json::json;

    fn backend(path: Option<&str>, kind: &str, message: &str) -> SettingProblem {
        SettingProblem {
            path: path.map(str::to_string),
            kind: kind.into(),
            message: message.into(),
            hint: None,
        }
    }

    #[test]
    fn empty_required_list_produces_an_empty_list_problem() {
        let catalog = fx::sample_catalog();
        let doc = fx::empty_doc();
        let problems = local_problems(&doc, &[(Root::Gateway, &catalog)]);
        assert!(
            problems
                .iter()
                .any(|p| { p.path.as_deref() == Some("providers") && p.kind == "empty_list" })
        );
    }

    #[test]
    fn short_list_uses_invalid_value_and_reports_the_count() {
        let mut catalog = fx::sample_catalog();
        catalog.children[2].min_items = Some(3);
        let doc = fx::sample_doc(); // tools = ["Bash", "Read"]
        let problems = local_problems(&doc, &[(Root::Gateway, &catalog)]);
        let tools = problems
            .iter()
            .find(|p| p.path.as_deref() == Some("tools"))
            .expect("tools 违规");
        assert_eq!(tools.kind, "invalid_value");
        assert_eq!(tools.message, "至少需要 3 项（当前 2 项）");
    }

    #[test]
    fn missing_required_leaves_are_reported_including_inside_list_items() {
        let catalog = fx::sample_catalog();
        let doc = SettingsDoc::new(
            json!({"providers": [{"name": "ok"}, {"protocol": "openai"}]}),
            json!({}),
            std::collections::HashMap::new(),
            "fp".into(),
        );
        let problems = local_problems(&doc, &[(Root::Gateway, &catalog)]);
        // providers[0] 缺 base_url；providers[1] 缺 name 与 base_url。
        assert!(
            problems
                .iter()
                .any(|p| p.path.as_deref() == Some("providers[0].base_url"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.path.as_deref() == Some("providers[1].name"))
        );
        assert!(
            !problems
                .iter()
                .any(|p| p.path.as_deref() == Some("providers[0].name"))
        );
        // provider 的 models 是 min_items: 1 → 两处都空。
        assert!(
            problems
                .iter()
                .any(|p| p.path.as_deref() == Some("providers[1].models"))
        );
    }

    #[test]
    fn missing_required_secret_is_reported_and_present_ones_are_not() {
        let mut provider = fx::object("p", vec![fx::secret_field("api_key")]);
        provider.children[0].required = true;
        let catalog = fx::root(vec![fx::list("ps", fx::element(provider))]);
        // 秘密字段存在（哪怕空串）→ 不报。
        let doc = SettingsDoc::new(
            json!({"ps": [{"api_key": ""}]}),
            json!({}),
            std::collections::HashMap::new(),
            "fp".into(),
        );
        let problems = local_problems(&doc, &[(Root::Gateway, &catalog)]);
        assert!(problems.is_empty(), "{problems:?}");
        // 元素里缺字段 → 报一条 missing_required。
        let doc = SettingsDoc::new(
            json!({"ps": [{}]}),
            json!({}),
            std::collections::HashMap::new(),
            "fp".into(),
        );
        let problems = local_problems(&doc, &[(Root::Gateway, &catalog)]);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].path.as_deref(), Some("ps[0].api_key"));
        assert_eq!(problems[0].kind, "missing_required");
        // 列表空 → 只有 ps 自己的空列表问题，没有实例级问题。
        let doc = fx::empty_doc();
        let problems = local_problems(&doc, &[(Root::Gateway, &catalog)]);
        assert!(
            problems
                .iter()
                .all(|p| p.path.as_deref() != Some("ps[0].api_key"))
        );
    }

    #[test]
    fn readonly_nodes_produce_no_local_problems() {
        let mut provider = fx::object("p", vec![fx::required_str("name")]);
        provider.editable = false;
        let catalog = fx::root(vec![fx::list("ps", fx::element(provider))]);
        let doc = SettingsDoc::new(
            json!({"ps": [{}]}),
            json!({}),
            std::collections::HashMap::new(),
            "fp".into(),
        );
        assert!(local_problems(&doc, &[(Root::Gateway, &catalog)]).is_empty());
    }

    #[test]
    fn merge_deduplicates_by_path_and_kind_and_takes_the_backend_message() {
        let local = vec![{
            let mut p = fx::problem(
                Root::Gateway,
                Some("providers[0].name"),
                "missing_required",
                "本地文案",
            );
            p.hint = Some("本地提示".into());
            p
        }];
        let backend = vec![
            backend(Some("providers[0].name"), "missing_required", "后端文案"),
            backend(
                Some("providers[0].base_url"),
                "missing_required",
                "必填项未设置",
            ),
        ];
        let merged = merge(local, &backend);
        assert_eq!(merged.len(), 2);
        let name = merged
            .iter()
            .find(|p| p.path.as_deref() == Some("providers[0].name"))
            .unwrap();
        assert_eq!(name.message, "后端文案", "AD4：message 权威在后端");
        assert_eq!(
            name.hint.as_deref(),
            Some("本地提示"),
            "AD4：后端没带 hint → 本地兜底，去重不许把建议丢掉"
        );
    }

    #[test]
    fn merge_prefers_the_backend_hint_when_it_has_one() {
        let local = vec![{
            let mut p = fx::problem(
                Root::Gateway,
                Some("providers[0].name"),
                "duplicate",
                "本地",
            );
            p.hint = Some("本地提示".into());
            p
        }];
        let mut incoming = backend(Some("providers[0].name"), "duplicate", "后端");
        incoming.hint = Some("给其中一个声明显式 id".into());
        let merged = merge(local, &[incoming]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].hint.as_deref(), Some("给其中一个声明显式 id"));
    }

    #[test]
    fn merge_keeps_different_kinds_on_the_same_path_apart() {
        let local = vec![fx::problem(
            Root::Gateway,
            Some("providers"),
            "empty_list",
            "a",
        )];
        let backend = vec![backend(Some("providers"), "missing_required", "b")];
        assert_eq!(
            merge(local, &backend).len(),
            2,
            "同路径不同 kind 不是同一条"
        );
    }

    #[test]
    fn merge_sorts_by_severity_then_path_and_puts_unknown_last() {
        let local = vec![
            fx::problem(Root::Gateway, Some("b"), "empty_list", "x"),
            fx::problem(Root::Gateway, Some("a"), "missing_required", "x"),
            fx::problem(Root::Gateway, Some("c"), "weird_unknown", "x"),
            fx::problem(Root::Gateway, None, "invalid_value", "文档级"),
            fx::problem(Root::Gateway, Some("d"), "unknown_key", "x"),
        ];
        let merged = merge(local, &[]);
        let kinds: Vec<&str> = merged.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "missing_required",
                "empty_list",
                "invalid_value",
                "unknown_key",
                "weird_unknown"
            ],
            "未知种类排最后；None 路径在同档里排最后（invalid_value 只有它）"
        );
    }

    #[test]
    fn none_path_sorts_last_within_the_same_severity() {
        let local = vec![
            fx::problem(Root::Gateway, None, "invalid_value", "doc-level"),
            fx::problem(Root::Gateway, Some("z"), "invalid_value", "z"),
        ];
        let merged = merge(local, &[]);
        assert_eq!(merged[0].path.as_deref(), Some("z"));
        assert_eq!(merged[1].path, None);
    }

    #[test]
    fn backend_problems_land_on_the_gateway_root() {
        let merged = merge(vec![], &[backend(None, "conflict", "外部修改")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].root, Root::Gateway);
    }
}
