//! `wing config list` / `get`：目录寻址、行构建、密文脱敏与两种输出形态。

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use wing_api_client::models::{SecretState, SettingNode, SettingProblem, parse_path};

use super::Outcome;
use super::SettingsApi;
use super::catalog::{ListRow, fetch_catalog, redact_secrets, sorted_problems};
use super::path::{normalize_steps, resolve_node};
use super::render::json_string;
use super::render::{
    VALUE_MAX_CHARS, apply_short, render_constraints, render_json_value, render_json_value_full,
    render_list_text, render_secret,
};
use crate::shared::doc_edit;

// ============================================================
// list / get（读路径）
// ============================================================

/// `wing config list`
pub(super) async fn execute_list<B: SettingsApi>(
    backend: &B,
    json: bool,
    http_base: &str,
    section: Option<&str>,
    only_overridden: bool,
) -> Outcome {
    let (schema, current) = match fetch_catalog(backend, http_base).await {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };
    let rows = match build_rows(
        &schema.root,
        &current.values,
        &current.secrets,
        &current.problems,
        section,
        only_overridden,
    ) {
        Ok(rows) => rows,
        Err(message) => return Outcome::usage(&message),
    };

    if json {
        let output = ListOutput {
            config_path: current.config_path.clone(),
            fingerprint: current.fingerprint.clone(),
            setup_mode: current.setup_mode,
            // 文档级问题（path=null，如 YAML 语法错）挂不到任何行上，单列一份全量清单。
            problems: sorted_problems(&current.problems)
                .into_iter()
                .cloned()
                .collect(),
            rows,
        };
        Outcome::ok(format!("{}\n", json_string(&output)))
    } else {
        Outcome::ok(render_list_text(&rows))
    }
}

/// `--json` 输出形状：`wing config list`（树已拍平成行，顺序 = 文本顺序）。
#[derive(Debug, Serialize)]
pub(super) struct ListOutput {
    config_path: String,
    fingerprint: String,
    setup_mode: bool,
    /// 全量问题清单（按严重度排序；行上另有一个 `problem` 便于就地标红）。
    problems: Vec<SettingProblem>,
    rows: Vec<ListRow>,
}

/// `wing config get`
pub(super) async fn execute_get<B: SettingsApi>(
    backend: &B,
    json: bool,
    http_base: &str,
    path: &str,
) -> Outcome {
    let (schema, current) = match fetch_catalog(backend, http_base).await {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };

    let Some(steps) = parse_path(path) else {
        return Outcome::usage(&format!(
            "路径文法非法：`{path}`（示例：gateway.port / providers[0].api_key）"
        ));
    };
    // 先剥可选根前缀，之后目录寻址、文档读取、密文表 / 问题表查找与回显都用规范路径。
    let steps = match normalize_steps(&schema.root, steps) {
        Ok(steps) => steps,
        Err(message) => return Outcome::usage(&message),
    };
    let canonical = doc_edit::format_path(&steps);
    let Some(node) = resolve_node(&schema.root, &current.values, &steps) else {
        return Outcome::usage(&format!(
            "路径不在设置目录中：`{canonical}`（用 `wing config list` 查看全部路径）"
        ));
    };

    let value = doc_edit::get_path(&current.values, &steps).cloned();
    let secret = current.secrets.get(&canonical).cloned();
    let problem = sorted_problems(&current.problems)
        .into_iter()
        .find(|p| p.path.as_deref() == Some(canonical.as_str()))
        .cloned();
    let output = GetOutput::new(&canonical, node, value, secret, problem);

    if json {
        Outcome::ok(format!("{}\n", json_string(&output)))
    } else {
        Outcome::ok(output.render_text())
    }
}

/// `--json` 输出形状：`wing config get`。
#[derive(Debug, Serialize)]
pub(super) struct GetOutput {
    pub(super) path: String,
    pub(super) kind: String,
    pub(super) apply: String,
    /// 生效值：文档里有就是文档值，否则回落声明默认值；都没有是 `null`。
    pub(super) value: Option<Value>,
    pub(super) overridden: bool,
    pub(super) has_default: bool,
    pub(super) default: Option<Value>,
    pub(super) secret: Option<SecretState>,
    pub(super) problem: Option<SettingProblem>,
    /// 目录节点全量（协议类型原样；零映射零漂移）。
    pub(super) node: SettingNode,
}

impl GetOutput {
    pub(super) fn new(
        path: &str,
        node: &SettingNode,
        value: Option<Value>,
        secret: Option<SecretState>,
        problem: Option<SettingProblem>,
    ) -> Self {
        let overridden = value.is_some();
        // 防线：即便上游没有掩码，密文也不会经本 CLI 的任一输出流出。
        let value = value.map(|value| redact_secrets(node, &value));
        let default = if node.secret {
            None
        } else {
            node.default.clone()
        };
        let effective = value
            .clone()
            .or_else(|| default.clone().filter(|_| node.has_default));
        Self {
            path: path.to_string(),
            kind: node.kind.as_str().to_string(),
            apply: node.apply.as_str().to_string(),
            value: effective,
            overridden,
            has_default: node.has_default,
            default,
            secret,
            problem,
            node: redact_node_defaults(node),
        }
    }

    /// 人类可读输出（纯函数，便于断言）。
    pub(super) fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{} = {}\n", self.path, self.render_value_column()));
        let default_column = match (&self.default, self.has_default) {
            (Some(value), true) => format!("默认 {}", render_json_value(value)),
            _ => "默认 (无)".to_string(),
        };
        let mut flags = format!("{default_column} · 生效域 {}", apply_short(&self.apply));
        if self.overridden {
            flags.push_str(" · 已覆盖");
        }
        if self.node.required {
            flags.push_str(" · 必填");
        }
        out.push_str(&format!("  {flags}\n"));
        out.push_str(&format!("  {}\n", self.node.doc));
        for note in self.node.notes.iter().filter(|n| !n.trim().is_empty()) {
            out.push_str(&format!("  {note}\n"));
        }
        if let Some(example) = self.node.example.as_deref() {
            out.push_str(&format!("  示例：{example}\n"));
        }
        let constraints = render_constraints(&self.node);
        if !constraints.is_empty() {
            out.push_str(&format!("  约束：{}\n", constraints.join(" · ")));
        }
        if !self.node.choices.is_empty() {
            let choices: Vec<String> = self
                .node
                .choices
                .iter()
                .map(
                    |choice| match choice.doc.as_deref().filter(|d| !d.trim().is_empty()) {
                        Some(doc) => format!("{}（{doc}）", choice.value),
                        None => choice.value.clone(),
                    },
                )
                .collect();
            out.push_str(&format!("  可选值：{}\n", choices.join(" / ")));
        }
        if let Some(problem) = &self.problem {
            out.push_str(&format!("  ! {}: {}\n", problem.kind, problem.message));
            if let Some(hint) = problem.hint.as_deref().filter(|h| !h.trim().is_empty()) {
                out.push_str(&format!("    ↳ {hint}\n"));
            }
        }
        out
    }

    /// 值列：密文一律掩码（真实值不出网关，也绝不到终端）。
    fn render_value_column(&self) -> String {
        if let Some(secret) = &self.secret {
            return render_secret(secret);
        }
        if self.node.is_structural() {
            return format!("({})", self.kind);
        }
        match &self.value {
            Some(value) => {
                let rendered = render_json_value(value);
                if render_json_value_full(value).chars().count() > VALUE_MAX_CHARS {
                    // 单项 get 被截断必须标注：完整值在 --json 里（list 的截断是版面设计，不标注）。
                    format!("{rendered}  （已截断，完整值用 --json 取）")
                } else {
                    rendered
                }
            }
            None => "(未设置)".to_string(),
        }
    }
}

/// 把目录 + 稀疏文档 + 密文表 + 问题表拍平成 `list` 的行（纯函数）。
pub(super) fn build_rows(
    root: &SettingNode,
    values: &Value,
    secrets: &HashMap<String, SecretState>,
    problems: &[SettingProblem],
    section: Option<&str>,
    only_overridden: bool,
) -> Result<Vec<ListRow>, String> {
    let sections: Vec<String> = root
        .children
        .iter()
        .filter_map(|child| child.section.clone())
        .collect();
    if let Some(wanted) = section {
        let known = sections.iter().any(|s| s.eq_ignore_ascii_case(wanted));
        if !known {
            return Err(format!(
                "未知分组 `{wanted}`（可选：{}）",
                if sections.is_empty() {
                    "(无)".to_string()
                } else {
                    sections.join(" / ")
                }
            ));
        }
    }

    let ctx = RowCtx {
        secrets,
        problems: sorted_problems(problems),
        only_overridden,
    };
    let mut rows = Vec::new();
    for (index, child) in root.children.iter().enumerate() {
        let child_section = child.section.clone();
        if let Some(wanted) = section {
            let matches = child_section
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(wanted));
            if !matches {
                continue;
            }
        }
        let value = parse_path(&child.path).and_then(|steps| doc_edit::get_path(values, &steps));
        let last = index + 1 == root.children.len();
        walk_node(
            child,
            &child.path,
            value,
            0,
            last,
            child_section,
            &ctx,
            &mut rows,
        );
    }
    Ok(rows)
}

/// `build_rows` 的只读上下文。
pub(super) struct RowCtx<'a> {
    secrets: &'a HashMap<String, SecretState>,
    problems: Vec<&'a SettingProblem>,
    only_overridden: bool,
}

/// 深度优先（声明序）拍平一个节点；列表按文档实际值展开具体元素。
#[allow(clippy::too_many_arguments)]
pub(super) fn walk_node(
    node: &SettingNode,
    path: &str,
    value: Option<&Value>,
    depth: usize,
    last: bool,
    section: Option<String>,
    ctx: &RowCtx<'_>,
    out: &mut Vec<ListRow>,
) {
    if !ctx.only_overridden || value.is_some() {
        out.push(make_row(
            node,
            path,
            value,
            depth,
            last,
            section.clone(),
            ctx,
        ));
    }

    // 结构节点下钻：object → children；list → 文档里的具体元素（模板路径无法展开）。
    if !node.children.is_empty() {
        let last_index = node.children.len() - 1;
        for (index, child) in node.children.iter().enumerate() {
            let child_path = format!("{path}.{}", child.key);
            let child_value = value.and_then(|v| v.get(&child.key));
            walk_node(
                child,
                &child_path,
                child_value,
                depth + 1,
                index == last_index,
                section.clone(),
                ctx,
                out,
            );
        }
        return;
    }

    if node.is_structural() {
        let Some(array) = value.and_then(|v| v.as_array()) else {
            return;
        };
        let last_index = array.len().saturating_sub(1);
        for (index, item) in array.iter().enumerate() {
            let item_path = format!("{path}[{index}]");
            let Some(element) = element_node_for(node, item) else {
                continue;
            };
            walk_node(
                element,
                &item_path,
                Some(item),
                depth + 1,
                index == last_index,
                section.clone(),
                ctx,
                out,
            );
        }
    }
}

/// 列表元素节点：单一元素类型走 `element`；union 元素按文档实际值选形态（`select_variant`）。
pub(super) fn element_node_for<'a>(
    list: &'a SettingNode,
    value: &Value,
) -> Option<&'a SettingNode> {
    if let Some(element) = list.element.as_deref() {
        return Some(element);
    }
    list.select_variant(value)
}

pub(super) fn make_row(
    node: &SettingNode,
    path: &str,
    value: Option<&Value>,
    depth: usize,
    last: bool,
    section: Option<String>,
    ctx: &RowCtx<'_>,
) -> ListRow {
    let structural = node.is_structural();
    let secret = ctx.secrets.get(path).cloned();
    let problem = ctx
        .problems
        .iter()
        .find(|p| p.path.as_deref() == Some(path))
        .map(|p| (*p).clone());
    // 密文行不给值也不给默认值（真实值不出网关，也绝不到任何输出流）。
    let masked = secret.is_some() || node.secret;
    ListRow {
        path: path.to_string(),
        kind: node.kind.as_str().to_string(),
        apply: node.apply.as_str().to_string(),
        overridden: value.is_some(),
        value: if masked || structural {
            None
        } else {
            value.cloned()
        },
        has_default: node.has_default,
        default: if masked { None } else { node.default.clone() },
        secret,
        problem,
        structural,
        section,
        depth,
        last,
    }
}

/// 按目录把密文节点的 `default` 置空（纵深防御：`get --json` 带完整 `node`，
/// 若网关哪天给 secret 声明了默认值，它不该从这条路径流出去）。递归到 children / element / variants。
pub(super) fn redact_node_defaults(node: &SettingNode) -> SettingNode {
    let mut redacted = node.clone();
    if node.secret {
        redacted.default = None;
    }
    redacted.children = node.children.iter().map(redact_node_defaults).collect();
    redacted.element = node
        .element
        .as_deref()
        .map(|element| Box::new(redact_node_defaults(element)));
    redacted.variants = node
        .variants
        .as_ref()
        .map(|variants| variants.iter().map(redact_node_defaults).collect());
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::config::ConfigCommand;
    use crate::cmd::config::testkit::*;
    use serde_json::{Value, json};
    use wing_api_client::models::{SecretPresence, SecretState};

    // ------------------------------------------------------------
    // 渲染
    // ------------------------------------------------------------

    #[test]
    fn render_list_text_shows_sections_tree_flags_and_problem_marker() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "providers[0].api_key".to_string(),
            SecretState {
                state: SecretPresence::Set,
                hint: Some("0001".to_string()),
            },
        );
        let problems = vec![problem(
            Some("providers[0].protocol"),
            "invalid_value",
            "protocol 不认识",
            None,
        )];
        let rows = build_rows(&catalog(), &document(), &secrets, &problems, None, false).unwrap();
        let text = render_list_text(&rows);

        assert!(text.contains("── Providers ──"), "{text}");
        assert!(text.contains("── Gateway ──"), "{text}");
        assert!(text.contains("providers  (list)"), "{text}");
        // ├─ / └─ 连接符 + 祖先竖线（深度优先顺序下的树形）。
        assert!(text.contains("│  └─ providers[0]  (object)"), "{text}");
        assert!(
            text.contains("│     ├─ providers[0].name = qoder"),
            "{text}"
        );
        assert!(
            text.contains("│     └─ providers[0].extra_body = (未设置)"),
            "{text}"
        );
        assert!(
            text.contains("│     ├─ providers[0].models  (list)"),
            "{text}"
        );
        assert!(text.contains("providers[0].name = qoder"), "{text}");
        assert!(
            text.contains("providers[0].api_key = •••••••• 0001"),
            "{text}"
        );
        assert!(text.contains("gateway.port = 32523  默认 32523"), "{text}");
        assert!(text.contains("! providers[0].protocol = openai"), "{text}");
        assert!(text.contains("[hot]"), "{text}");
        assert!(text.contains("[restart]"), "{text}");
        assert!(text.contains("已覆盖"), "{text}");
        // 列表元素逐个展开。
        assert!(text.contains("providers[0].models[0] = ds-flash"), "{text}");
        assert!(
            text.contains("providers[0].models[1].id = ds-pro"),
            "{text}"
        );
        assert!(text.contains("agents[0].name = default"), "{text}");
        assert!(text.contains("agents[0].model = ds-flash"), "{text}");
    }

    #[test]
    fn build_rows_only_overridden_keeps_document_paths_only() {
        let rows = build_rows(&catalog(), &document(), &HashMap::new(), &[], None, true).unwrap();
        let paths: Vec<&str> = rows.iter().map(|row| row.path.as_str()).collect();
        assert!(paths.contains(&"providers[0].base_url"), "{paths:?}");
        assert!(paths.contains(&"gateway.port"), "{paths:?}");
        assert!(
            !paths.contains(&"gateway.host"),
            "默认值路径必须隐藏：{paths:?}"
        );
        assert!(
            !paths.contains(&"images"),
            "无覆盖的分组必须隐藏：{paths:?}"
        );
    }

    #[test]
    fn build_rows_section_filter_matches_case_insensitively() {
        let rows = build_rows(
            &catalog(),
            &document(),
            &HashMap::new(),
            &[],
            Some("gateway"),
            false,
        )
        .unwrap();
        let paths: Vec<&str> = rows.iter().map(|row| row.path.as_str()).collect();
        assert!(
            paths.iter().all(|path| path.starts_with("gateway")),
            "{paths:?}"
        );
        assert!(paths.contains(&"gateway.auth.enabled"), "{paths:?}");

        let message = build_rows(
            &catalog(),
            &document(),
            &HashMap::new(),
            &[],
            Some("nope"),
            false,
        )
        .unwrap_err();
        assert!(message.contains("未知分组"), "{message}");
        assert!(
            message.contains("Providers"),
            "错误里要列出可选分组：{message}"
        );
    }

    #[test]
    fn render_list_text_says_so_when_nothing_matches() {
        assert_eq!(render_list_text(&[]), "（没有匹配的设置项）\n");
        // --only-overridden 在空文档上就是这一行。
        let rows = build_rows(&catalog(), &json!({}), &HashMap::new(), &[], None, true).unwrap();
        assert!(rows.is_empty());
        assert_eq!(render_list_text(&rows), "（没有匹配的设置项）\n");
    }

    #[test]
    fn list_json_rows_carry_snake_case_fields_and_null_secret_values() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "providers[0].api_key".to_string(),
            SecretState {
                state: SecretPresence::Set,
                hint: Some("0001".to_string()),
            },
        );
        let mut values = document();
        values["providers"][0]["api_key"] = json!("sk-plain-0001");
        let rows = build_rows(&catalog(), &values, &secrets, &[], None, false).unwrap();
        let api_key_row = rows
            .iter()
            .find(|row| row.path == "providers[0].api_key")
            .expect("secret row");
        assert_eq!(api_key_row.value, None, "密文行不能带值");
        assert_eq!(api_key_row.default, None);
        assert_eq!(api_key_row.kind, "secret");
        assert_eq!(
            api_key_row.secret.as_ref().unwrap().hint.as_deref(),
            Some("0001")
        );
    }

    #[test]
    fn get_output_renders_effective_default_and_overridden_flag() {
        let root = catalog();
        let doc = json!({"gateway": {"port": 8080}});

        let node = resolve_node(&root, &doc, &steps("gateway.port")).unwrap();
        let output = GetOutput::new("gateway.port", node, Some(json!(8080)), None, None);
        assert!(output.overridden);
        assert_eq!(output.value, Some(json!(8080)));
        let text = output.render_text();
        assert!(text.starts_with("gateway.port = 8080\n"), "{text}");
        assert!(text.contains("默认 32523"), "{text}");
        assert!(text.contains("已覆盖"), "{text}");
        assert!(text.contains("约束：≥ 1"), "{text}");

        // 未覆盖 → 值回落默认。
        let node = resolve_node(&root, &doc, &steps("gateway.host")).unwrap();
        let output = GetOutput::new("gateway.host", node, None, None, None);
        assert!(!output.overridden);
        assert_eq!(output.value, Some(json!("127.0.0.1")));
        assert!(
            output.render_text().contains("gateway.host = 127.0.0.1"),
            "{}",
            output.render_text()
        );

        // 必填但没有默认值 → 未设置。
        let node = resolve_node(&root, &doc, &steps("providers[0].base_url")).unwrap();
        let output = GetOutput::new("providers[0].base_url", node, None, None, None);
        assert_eq!(output.value, None);
        let text = output.render_text();
        assert!(text.contains("(未设置)"), "{text}");
        assert!(text.contains("必填"), "{text}");
    }

    /// 把目录里的 `providers[].api_key` 改成"带默认值的密文"（对抗性 fixture：真实声明没有默认值）。
    fn catalog_with_secret_default(secret_default: &str) -> SettingNode {
        let mut root = catalog();
        let provider = root.children[0]
            .element
            .as_mut()
            .expect("providers 元素")
            .as_mut();
        let key = provider
            .children
            .iter_mut()
            .find(|child| child.key == "api_key")
            .expect("api_key");
        key.default = Some(json!(secret_default));
        key.has_default = true;
        root
    }

    /// `get --json` 的 `node` 是完整目录节点——密文节点的 `default` 也必须置空，
    /// hint 之外的任何密钥形态都不许从这条路径流出去。
    #[tokio::test]
    async fn get_json_never_carries_a_secret_default() {
        const SECRET_DEFAULT: &str = "sk-default-should-not-leak";
        let backend = FakeBackend::healthy(catalog_with_secret_default(SECRET_DEFAULT), document());
        let (code, stdout, stderr) = run_cmd(
            &backend,
            ConfigCommand::Get {
                path: "providers[0].api_key".to_string(),
            },
            true,
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
        assert!(!stdout.contains(SECRET_DEFAULT), "默认值泄露：{stdout}");
        let parsed: Value = serde_json::from_str(stdout.trim()).expect("json");
        assert_eq!(parsed["default"], Value::Null, "顶层 default 必须为空");
        assert_eq!(
            parsed["node"]["default"],
            Value::Null,
            "node.default 必须为空"
        );
        assert_eq!(parsed["node"]["has_default"], json!(true));

        // 同一份目录里的非密文字段不受影响（默认值照常展示）。
        let (_, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Get {
                path: "gateway.port".to_string(),
            },
            true,
        )
        .await;
        let parsed: Value = serde_json::from_str(stdout.trim()).expect("json");
        assert_eq!(parsed["node"]["default"], json!(32523));
    }

    /// `get --json` 是编排器消费的最富形状（含完整 `node`）——钉住字段名与递归结构。
    #[tokio::test]
    async fn get_json_shape_is_stable_and_node_is_recursive() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, stdout, stderr) = run_cmd(
            &backend,
            ConfigCommand::Get {
                path: "providers[0]".to_string(),
            },
            true,
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
        let parsed: Value = serde_json::from_str(stdout.trim()).expect("get --json parses");
        for field in [
            "path",
            "kind",
            "apply",
            "value",
            "overridden",
            "has_default",
            "default",
            "secret",
            "problem",
            "node",
        ] {
            assert!(parsed.get(field).is_some(), "缺少字段 {field}：{parsed}");
        }
        assert_eq!(parsed["path"], json!("providers[0]"));
        assert_eq!(parsed["kind"], json!("object"));
        assert_eq!(parsed["node"]["key"], json!("[]"));
        assert_eq!(parsed["node"]["path"], json!("providers[]"));
        // node 递归：子节点带自己的 path/kind/约束。
        let children = parsed["node"]["children"].as_array().expect("children");
        let api_key = children
            .iter()
            .find(|child| child["key"] == json!("api_key"))
            .expect("api_key 子节点");
        assert_eq!(api_key["kind"], json!("secret"));
        assert_eq!(api_key["secret"], json!(true));
        assert_eq!(api_key["path"], json!("providers[].api_key"));
        // 列表元素模板也递归带出来（union variants）。
        let models = children
            .iter()
            .find(|child| child["key"] == json!("models"))
            .expect("models 子节点");
        assert_eq!(models["element"], Value::Null);
        assert_eq!(
            models["variants"].as_array().expect("variants").len(),
            2,
            "union 元素的两个形态都要在 node 里"
        );
    }

    /// 单项 `get` 的值被截断时必须标注（完整值在 `--json` 里）。
    #[test]
    fn get_text_marks_truncated_values() {
        let root = catalog();
        let mut doc = document();
        let long = "x".repeat(VALUE_MAX_CHARS + 20);
        doc["providers"][0]["base_url"] = json!(long);
        let node = resolve_node(&root, &doc, &steps("providers[0].base_url")).unwrap();
        let output = GetOutput::new("providers[0].base_url", node, Some(json!(long)), None, None);
        let text = output.render_text();
        assert!(text.contains("..."), "{text}");
        assert!(text.contains("已截断，完整值用 --json 取"), "{text}");
        // 短值不加标注。
        let output = GetOutput::new(
            "providers[0].base_url",
            node,
            Some(json!("https://example.com/v1")),
            None,
            None,
        );
        assert!(
            !output.render_text().contains("已截断"),
            "{}",
            output.render_text()
        );
    }
}
