//! `wing config set|unset|add|remove|move`：写路径的校验、计划与执行。
//!
//! 结构是"文法 → 规范化 → 计划 → 一次性 POST"：本地先把整份目标文档算出来（`Plan`），
//! 断言它只改动预期的路径，再提交；网关侧仍然全有或全无。

use serde::Serialize;
use serde_json::{Value, json};
use wing_api_client::models::{SettingKind, SettingNode, SettingsSetRequest, SettingsSetResponse};

use super::Outcome;
use super::SettingsApi;
use super::api_failure;
use super::catalog::{fetch_catalog, redact_secrets, sorted_problems};
use super::path::{
    Incoming, coerce_value, ensure_exclusive, incoming_json, incoming_value, parse_json,
    plan_steps, removal_element, require_node, union_replacement_target, write_steps,
};
use super::render::json_string;
use super::render::render_json_value;
use crate::shared::doc_edit;
use crate::shared::doc_edit::Policy;

// ============================================================
// 写路径（set / unset / add / remove / move）
// ============================================================

/// 一条写命令（已解析的参数）。
#[derive(Debug)]
pub(super) enum WriteOp {
    Set {
        path: String,
        value: Option<String>,
        json_value: Option<String>,
    },
    Unset {
        path: String,
    },
    Add {
        path: String,
        value: Option<String>,
        json_value: Option<String>,
        variant: Option<String>,
    },
    Remove {
        path: String,
    },
    Move {
        path: String,
        delta: i64,
    },
}

/// 本地规划的结论：要么提交一份新文档，要么无操作短路。
#[derive(Debug)]
pub(super) enum Plan {
    Save {
        document: Value,
        note: Option<String>,
    },
    Noop {
        reason: &'static str,
        message: String,
    },
}

/// 写命令的统一骨架：读 → 本地改 → 保存（`--force` 时 `base: null` 跳过指纹检查）。
pub(super) async fn execute_write<B: SettingsApi>(
    op: WriteOp,
    json: bool,
    backend: &B,
    http_base: &str,
    force: bool,
) -> Outcome {
    // 先做不联网的输入校验：语法 / 参数互斥 / --variant 词表 / --json-value 语法。
    // 这样用法错误（4）在网关不可达时也能正确报出，而不是先撞上"网关不可达"。
    if let Err(message) = precheck_write(&op) {
        return Outcome::usage(&message);
    }

    let (schema, current) = match fetch_catalog(backend, http_base).await {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };

    let plan = match plan_write(&op, &schema.root, &current.values) {
        Ok(plan) => plan,
        Err(message) => return Outcome::usage(&message),
    };

    let (document, note) = match plan {
        Plan::Save { document, note } => (document, note),
        Plan::Noop { reason, message } => {
            return if json {
                let output = WriteOutput {
                    ok: true,
                    noop: Some(reason.to_string()),
                    note: Some(message),
                    response: None,
                };
                Outcome::ok(format!("{}\n", json_string(&output)))
            } else {
                Outcome::ok(format!("（无操作）{message}\n"))
            };
        }
    };

    // `--force` = 跳过指纹检查（`base: null`）。
    let request = SettingsSetRequest {
        base: if force {
            None
        } else {
            Some(current.fingerprint.clone())
        },
        document,
    };

    match backend.set(&request).await {
        Ok(response) => {
            let stdout = if json {
                let output = WriteOutput {
                    ok: response.ok,
                    noop: None,
                    note: note.clone(),
                    response: Some(response.clone()),
                };
                format!("{}\n", json_string(&output))
            } else {
                render_write_text(&response, note.as_deref())
            };
            if response.ok {
                Outcome::ok(stdout)
            } else {
                Outcome::refused(stdout)
            }
        }
        Err(e) => {
            let (code, message) = api_failure(&e, http_base, "保存配置失败");
            let stderr = match &note {
                Some(note) => format!("{message}\n（改动未写盘：{note}）\n"),
                None => format!("{message}\n"),
            };
            Outcome::failure(code, stderr)
        }
    }
}

/// `--json` 输出形状：写命令（`noop` 短路与成功/被拒共用）。
#[derive(Debug, Serialize)]
pub(super) struct WriteOutput {
    ok: bool,
    noop: Option<String>,
    note: Option<String>,
    response: Option<SettingsSetResponse>,
}

/// 写回执（纯函数，便于断言）：只打印路径与状态，**绝不打印值**（密文自然脱敏）。
fn render_write_text(response: &SettingsSetResponse, note: Option<&str>) -> String {
    let mut out = String::new();
    if !response.ok {
        out.push_str("✗ 保存被拒绝（配置校验未通过，未写盘）\n");
        for problem in sorted_problems(&response.problems) {
            let at = problem.path.as_deref().unwrap_or("(文档)");
            out.push_str(&format!("  {at}: {}\n", problem.message));
            if let Some(hint) = problem.hint.as_deref().filter(|h| !h.trim().is_empty()) {
                out.push_str(&format!("      ↳ {hint}\n"));
            }
        }
        out.push_str(&format!(
            "共 {} 个问题 · 运行 wing 打开设置面板，或 wing config set <path> <value>\n",
            response.problems.len()
        ));
        if let Some(note) = note {
            out.push_str(&format!("（本次改动未写盘：{note}）\n"));
        }
        return out;
    }

    out.push_str(&format!("✓ 已保存（{} 处变更）\n", response.changed.len()));
    for changed in &response.changed {
        out.push_str(&format!("  · {changed}\n"));
    }
    // warnings 不是 problems：保存已经成功，但这些是用户必须知道的副作用。
    // 最典型的一条 —— 原文件语法坏到读不出来时我们无法保留其中的密钥，
    // 不显式说出来，用户就会以为密钥还在（下一次调用 401 才发现）。
    for warning in &response.warnings {
        out.push_str(&format!("⚠ {warning}\n"));
    }
    if let Some(note) = note {
        out.push_str(&format!("  {note}\n"));
    }
    if !response.restart_required.is_empty() {
        out.push_str(&format!(
            "⚠ 需重启网关才生效：{}\n",
            response.restart_required.join(", ")
        ));
        out.push_str("  运行 wing stop && wing start（或在 TUI 设置面板里按 Ctrl+R）\n");
    }
    if let Some(reload) = &response.reload {
        if !reload.results.is_empty() {
            let summary: Vec<String> = reload
                .results
                .iter()
                .map(|item| format!("{} {}", item.name, if item.ok { "✓" } else { "✗" }))
                .collect();
            out.push_str(&format!("热重载：{}\n", summary.join(" · ")));
        }
        for item in reload.results.iter().filter(|item| !item.ok) {
            if let Some(detail) = item.detail.as_deref() {
                out.push_str(&format!("  ✗ {}：{detail}\n", item.name));
            }
        }
        if !reload.ok {
            out.push_str("⚠ 热重载未全部成功（配置已写盘且合法）\n");
        }
    }
    if response.setup_mode_exited {
        out.push_str("✓ 网关已转入正常模式\n");
    }
    if let Some(backup) = response.backup_path.as_deref() {
        out.push_str(&format!("备份：{backup}\n"));
    }
    out
}

/// 写命令的本地规划：纯函数（无 I/O、无网络），全部边界在这里裁定。
pub(super) fn plan_write(op: &WriteOp, root: &SettingNode, values: &Value) -> Result<Plan, String> {
    // 空文档（`values: null`）视作 `{}`：后端在空文件上返回的就是空对象，这里只做防御。
    let mut document = if values.is_null() {
        json!({})
    } else {
        values.clone()
    };
    // 稀疏文档里密文叶是 `null`（= 保留磁盘现值）；原样回传是契约。
    match op {
        WriteOp::Set {
            path,
            value,
            json_value,
        } => {
            let steps = plan_steps(root, path)?;
            let incoming = incoming_value(value.as_deref(), json_value.as_deref())?;
            // union 列表元素的整项替换：形态由**新值**决定（`{` 开头 = 完整形态）；
            // 其余路径的形态由文档现值决定（`resolve_node`）。
            let node = match union_replacement_target(root, &document, &steps, incoming) {
                Some(variant) => variant,
                None => require_node(root, &document, &steps)?,
            };
            let coerced = incoming_json(node, incoming)?;
            doc_edit::set_path(&mut document, &steps, coerced, Policy::Strict)?;
            Ok(Plan::Save {
                document,
                note: None,
            })
        }
        WriteOp::Unset { path } => {
            let steps = plan_steps(root, path)?;
            require_node(root, &document, &steps)?;
            if doc_edit::unset_path(&mut document, &steps, Policy::Strict)? {
                Ok(Plan::Save {
                    document,
                    note: None,
                })
            } else {
                Ok(Plan::Noop {
                    reason: "already_default",
                    message: format!(
                        "`{}` 本就不在文档里（已在默认值上）",
                        doc_edit::format_path(&steps)
                    ),
                })
            }
        }
        WriteOp::Add {
            path,
            value,
            json_value,
            variant,
        } => {
            let steps = plan_steps(root, path)?;
            let canonical = doc_edit::format_path(&steps);
            let list = require_node(root, &document, &steps)?;
            if list.kind != SettingKind::List {
                return Err(format!(
                    "`{canonical}` 不是列表（kind: {}）",
                    list.kind.as_str()
                ));
            }
            let choice = variant.as_deref().map(parse_variant).transpose()?;
            let element = add_element_node(list, choice)?;
            let item = match (value.as_deref(), json_value.as_deref()) {
                (Some(_), Some(_)) => {
                    return Err("位置参数 <value> 与 --json-value 不能同时给出".to_string());
                }
                (None, Some(raw)) => parse_json(raw)?,
                (Some(raw), None) => coerce_value(element, raw)?,
                (None, None) => match doc_edit::empty_value(element, Policy::Strict) {
                    Some(stub) => stub,
                    None => {
                        return Err(format!(
                            "`{canonical}` 的元素是标量（{}），新增时必须给 <value>（或对 object 形态用 --variant object 插骨架）",
                            element.kind.as_str()
                        ));
                    }
                },
            };
            let index = doc_edit::append_item(&mut document, &steps, list, item, Policy::Strict)?;
            Ok(Plan::Save {
                document,
                note: Some(format!("新增项：{canonical}[{index}]")),
            })
        }
        WriteOp::Remove { path } => {
            let steps = plan_steps(root, path)?;
            require_node(root, &document, &steps)?;
            // 摘要前先拿到元素节点：脱敏要按目录走（密文叶在输出里一律 null）。
            let element = removal_element(root, &document, &steps);
            let removed = doc_edit::remove_indexed(&mut document, &steps)?;
            let summary = match element {
                Some(element) => redact_secrets(element, &removed),
                None => removed,
            };
            Ok(Plan::Save {
                document,
                note: Some(format!(
                    "已移除：{} = {}",
                    doc_edit::format_path(&steps),
                    render_json_value(&summary)
                )),
            })
        }
        WriteOp::Move { path, delta } => {
            let steps = plan_steps(root, path)?;
            require_node(root, &document, &steps)?;
            let outcome = doc_edit::move_indexed(&mut document, &steps, *delta, Policy::Strict)?;
            if !outcome.moved {
                let reason = if *delta == 0 {
                    "delta_zero"
                } else {
                    "already_at_edge"
                };
                let message = format!(
                    "`{}` 已在边界（第 {} / {} 项），未移动",
                    doc_edit::format_path(&steps),
                    outcome.from + 1,
                    outcome.length
                );
                return Ok(Plan::Noop { reason, message });
            }
            Ok(Plan::Save {
                document,
                note: Some(format!(
                    "已移动：{} → [{}]",
                    doc_edit::format_path(&steps),
                    outcome.to
                )),
            })
        }
    }
}

/// 写法/输入层面的纯校验（不联网）——见 `execute_write` 的调用点。
pub(super) fn precheck_write(op: &WriteOp) -> Result<(), String> {
    match op {
        WriteOp::Set {
            path,
            value,
            json_value,
        } => {
            write_steps(path)?;
            let incoming = incoming_value(value.as_deref(), json_value.as_deref())?;
            if let Incoming::RawJson(raw) = incoming {
                parse_json(raw)?;
            }
        }
        WriteOp::Unset { path } | WriteOp::Remove { path } => {
            write_steps(path)?;
        }
        WriteOp::Add {
            path,
            value,
            json_value,
            variant,
        } => {
            write_steps(path)?;
            ensure_exclusive(value.as_deref(), json_value.as_deref())?;
            if let Some(raw) = json_value.as_deref() {
                parse_json(raw)?;
            }
            if let Some(variant) = variant.as_deref() {
                parse_variant(variant)?;
            }
        }
        WriteOp::Move { path, .. } => {
            write_steps(path)?;
        }
    }
    Ok(())
}

// ============================================================
// 值强制转换
// ============================================================

// ============================================================
// `--variant` 与元素形态
// ============================================================

/// `--variant` 的形态词表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VariantChoice {
    /// 简单形态（裸字符串等标量）。
    Simple,
    /// 完整形态（对象 / map）。
    Object,
}

impl VariantChoice {
    fn word(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Object => "object",
        }
    }
}

/// 解析 `--variant` 的取值（只接受 `simple` / `object`）。
pub(super) fn parse_variant(raw: &str) -> Result<VariantChoice, String> {
    match raw {
        "simple" => Ok(VariantChoice::Simple),
        "object" => Ok(VariantChoice::Object),
        other => Err(format!("--variant 只接受 simple|object，收到 `{other}`")),
    }
}

/// 节点在 `--variant` 语义下的形态：object / map = 完整形态，其余 = 简单形态。
pub(super) fn variant_class(node: &SettingNode) -> VariantChoice {
    match node.kind {
        SettingKind::Object | SettingKind::Map => VariantChoice::Object,
        _ => VariantChoice::Simple,
    }
}

/// 选新增项的元素节点：`--variant` 缺省时取声明序第一个（variants 列表的"简单形态"在前）。
pub(super) fn add_element_node(
    list: &SettingNode,
    choice: Option<VariantChoice>,
) -> Result<&SettingNode, String> {
    let candidates: Vec<&SettingNode> = match (list.element.as_deref(), list.variants.as_deref()) {
        (Some(element), _) => vec![element],
        (None, Some(variants)) if !variants.is_empty() => variants.iter().collect(),
        _ => {
            return Err(format!(
                "{} 没有声明元素形态（退化列表），无法新增",
                list.path
            ));
        }
    };
    match choice {
        None => Ok(candidates[0]),
        Some(wanted) => candidates
            .iter()
            .copied()
            .find(|node| variant_class(node) == wanted)
            .ok_or_else(|| {
                let available: Vec<&str> =
                    candidates.iter().map(|n| variant_class(n).word()).collect();
                format!(
                    "{} 没有 {} 形态（可用：{}）",
                    list.path,
                    wanted.word(),
                    available.join(" / ")
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::config::path::{normalize_steps, resolve_node};
    use crate::cmd::config::testkit::*;
    use crate::shared::doc_edit;
    use crate::shared::doc_edit::Policy;
    use serde_json::{Value, json};
    use wing_api_client::models::{ReloadResponse, ReloadResultItem};

    // ------------------------------------------------------------
    // 值强制转换表
    // ------------------------------------------------------------

    fn scalar(kind: SettingKind) -> SettingNode {
        node("x", "x", kind)
    }

    #[test]
    fn coercion_bool_accepts_the_documented_spellings() {
        let target = scalar(SettingKind::Bool);
        for raw in ["true", "TRUE", "1", "yes", "On"] {
            assert_eq!(coerce_value(&target, raw).unwrap(), json!(true), "{raw}");
        }
        for raw in ["false", "False", "0", "no", "off"] {
            assert_eq!(coerce_value(&target, raw).unwrap(), json!(false), "{raw}");
        }
        assert_eq!(
            coerce_value(&target, "maybe").unwrap_err(),
            "需要一个布尔值（true/false）"
        );
    }

    #[test]
    fn coercion_int_parses_and_rejects() {
        let target = scalar(SettingKind::Int);
        assert_eq!(coerce_value(&target, " 300 ").unwrap(), json!(300));
        assert_eq!(coerce_value(&target, "-7").unwrap(), json!(-7));
        assert_eq!(coerce_value(&target, "3.5").unwrap_err(), "需要一个整数");
        assert_eq!(coerce_value(&target, "abc").unwrap_err(), "需要一个整数");
    }

    #[test]
    fn coercion_float_parses_integer_text_as_float() {
        let target = scalar(SettingKind::Float);
        // "300" → 300.0（不是整数 300）：否则写进 float 字段的是 string-or-int 形状。
        assert_eq!(coerce_value(&target, "300").unwrap(), json!(300.0));
        assert_ne!(coerce_value(&target, "300").unwrap(), json!(300));
        assert_eq!(coerce_value(&target, "0.5").unwrap(), json!(0.5));
        assert_eq!(coerce_value(&target, "1e3").unwrap(), json!(1000.0));
        assert_eq!(coerce_value(&target, "NaN").unwrap_err(), "需要一个数字");
        assert_eq!(coerce_value(&target, "inf").unwrap_err(), "需要一个数字");
        assert_eq!(coerce_value(&target, "x").unwrap_err(), "需要一个数字");
    }

    #[test]
    fn coercion_str_and_secret_pass_through_trimmed() {
        let text = scalar(SettingKind::Str);
        assert_eq!(coerce_value(&text, "  hello  ").unwrap(), json!("hello"));
        let mut secret = scalar(SettingKind::Secret);
        secret.secret = true;
        assert_eq!(coerce_value(&secret, " sk-1 ").unwrap(), json!("sk-1"));
    }

    #[test]
    fn coercion_enum_requires_a_declared_choice() {
        let mut target = scalar(SettingKind::Enum);
        target.choices = vec![choice("low", "低"), choice("high", "高")];
        assert_eq!(coerce_value(&target, "low").unwrap(), json!("low"));
        assert_eq!(
            coerce_value(&target, "LOW").unwrap_err(),
            "可选值：low / high"
        );
    }

    #[test]
    fn coercion_containers_accept_json_of_matching_shape() {
        let map = scalar(SettingKind::Map);
        assert_eq!(
            coerce_value(&map, r#"{"thinking":{"type":"enabled"}}"#).unwrap(),
            json!({"thinking": {"type": "enabled"}})
        );
        assert_eq!(coerce_value(&map, "[1]").unwrap_err(), "需要一个 JSON 对象");
        assert!(
            coerce_value(&map, "{oops")
                .unwrap_err()
                .starts_with("不是合法 JSON")
        );

        let object = scalar(SettingKind::Object);
        assert_eq!(coerce_value(&object, "{}").unwrap(), json!({}));

        let list = scalar(SettingKind::List);
        assert_eq!(
            coerce_value(&list, r#"["a","b"]"#).unwrap(),
            json!(["a", "b"])
        );
        assert_eq!(coerce_value(&list, "{}").unwrap_err(), "需要一个 JSON 数组");
    }

    #[test]
    fn coercion_nullable_accepts_null_tilde_and_empty_string() {
        let mut nullable = scalar(SettingKind::Str);
        nullable.nullable = true;
        for raw in ["null", "~", "", "  "] {
            assert_eq!(
                coerce_value(&nullable, raw).unwrap(),
                Value::Null,
                "{raw:?}"
            );
        }
        // 空串要有办法表达"空字符串本身"：--json-value '""'。
        assert_eq!(
            parse_json(r#""""#).unwrap(),
            json!(""),
            "逃生舱必须能表达空字符串"
        );
    }

    #[test]
    fn coercion_null_is_not_special_on_non_nullable_fields() {
        let text = scalar(SettingKind::Str);
        assert_eq!(coerce_value(&text, "null").unwrap(), json!("null"));
        let number = scalar(SettingKind::Int);
        assert_eq!(coerce_value(&number, "null").unwrap_err(), "需要一个整数");
    }

    #[test]
    fn coercion_unknown_kind_points_at_json_value() {
        let unknown = scalar(SettingKind::Unknown("future".to_string()));
        let message = coerce_value(&unknown, "x").unwrap_err();
        assert!(message.contains("--json-value"), "{message}");
    }

    #[test]
    fn parse_json_reports_syntax_errors_without_echoing_content() {
        let message = parse_json("{nope}").unwrap_err();
        assert!(message.starts_with("不是合法 JSON"), "{message}");
        assert!(
            !message.contains("nope"),
            "解析错误不该回显原始内容：{message}"
        );
    }

    // ------------------------------------------------------------
    // 路径与目录寻址
    // ------------------------------------------------------------

    #[test]
    fn format_path_renders_canonical_paths() {
        assert_eq!(
            doc_edit::format_path(&steps("providers[0].models[2].id")),
            "providers[0].models[2].id"
        );
        assert_eq!(doc_edit::format_path(&steps("providers[]")), "providers[]");
        assert_eq!(doc_edit::format_path(&[]), "");
    }

    #[test]
    fn resolve_node_finds_concrete_and_template_paths() {
        let root = catalog();
        let doc = document();
        let concrete = resolve_node(&root, &doc, &steps("providers[0].api_key")).expect("concrete");
        assert_eq!(concrete.path, "providers[].api_key");
        assert_eq!(concrete.kind, SettingKind::Secret);
        // 模板路径（[]）在 catalog 上永远可寻址（读命令用来学约束）。
        let template = resolve_node(&root, &doc, &steps("providers[].api_key")).expect("template");
        assert_eq!(template.path, "providers[].api_key");
        assert!(resolve_node(&root, &doc, &steps("providers[0].nope")).is_none());
    }

    #[test]
    fn resolve_node_selects_union_variant_by_document_value() {
        let root = catalog();
        let doc = document();
        // 裸字符串元素 → str 形态；对象元素 → object 形态。
        let simple = resolve_node(&root, &doc, &steps("providers[0].models[0]")).expect("simple");
        assert_eq!(simple.kind, SettingKind::Str);
        let rich = resolve_node(&root, &doc, &steps("providers[0].models[1]")).expect("object");
        assert_eq!(rich.kind, SettingKind::Object);
        let rich_child =
            resolve_node(&root, &doc, &steps("providers[0].models[1].id")).expect("id");
        assert_eq!(rich_child.path, "providers[].models[].id");
        // 文档里没有的元素：union 形态无法判定（不猜）。
        assert!(resolve_node(&root, &doc, &steps("providers[0].models[9]")).is_none());
    }

    #[test]
    fn normalization_is_the_single_authority_for_the_root_prefix() {
        let root = catalog(); // root.key == "config"
        // 入口规范化：带/不带前缀 → 同一条规范路径。
        let plain = normalize_steps(&root, steps("gateway.port")).expect("plain");
        let prefixed = normalize_steps(&root, steps("config.gateway.port")).expect("prefixed");
        assert_eq!(doc_edit::format_path(&plain), "gateway.port");
        assert_eq!(doc_edit::format_path(&prefixed), "gateway.port");

        // 只敲根名 → 用法错误 + 顶层节名。
        let message = normalize_steps(&root, steps("config")).unwrap_err();
        assert!(message.contains("路径为空"), "{message}");
        assert!(message.contains("providers"), "{message}");

        // 根名才是唯一依据：别的顶层键不会被当前缀剥掉。
        let mut other = catalog();
        other.key = "settings".to_string();
        let kept = normalize_steps(&other, steps("config.gateway.port")).expect("kept");
        assert_eq!(doc_edit::format_path(&kept), "config.gateway.port");

        // resolve_node 只接受已规范化的 steps（容忍只在入口做一次，避免"两处各判一半"）。
        let doc = document();
        assert!(resolve_node(&root, &doc, &steps("gateway.port")).is_some());
        assert!(resolve_node(&root, &doc, &steps("config.gateway.port")).is_none());
        assert!(resolve_node(&root, &doc, &prefixed).is_some());
    }

    // ------------------------------------------------------------
    // 文档编辑（实现已下沉到 `shared::doc_edit`；这里钉 CLI 的用法错误文案）
    // ------------------------------------------------------------

    #[test]
    fn set_path_creates_missing_intermediate_objects() {
        let mut doc = json!({});
        doc_edit::set_path(
            &mut doc,
            &steps("gateway.auth.enabled"),
            json!(true),
            Policy::Strict,
        )
        .unwrap();
        assert_eq!(doc, json!({"gateway": {"auth": {"enabled": true}}}));
        // null 中间层同样被物化成对象（nullable object 字段）。
        let mut doc = json!({"gateway": {"auth": null}});
        doc_edit::set_path(
            &mut doc,
            &steps("gateway.auth.enabled"),
            json!(true),
            Policy::Strict,
        )
        .unwrap();
        assert_eq!(doc, json!({"gateway": {"auth": {"enabled": true}}}));
    }

    #[test]
    fn set_path_rejects_scalar_intermediate_and_out_of_range_index() {
        let mut doc = json!({"gateway": {"port": 8080}});
        let message = doc_edit::set_path(
            &mut doc,
            &steps("gateway.port.sub"),
            json!(1),
            Policy::Strict,
        )
        .unwrap_err();
        assert!(message.contains("不是对象"), "{message}");
        let message = doc_edit::set_path(
            &mut doc,
            &steps("gateway.port[0]"),
            json!(1),
            Policy::Strict,
        )
        .unwrap_err();
        assert!(message.contains("不是列表"), "{message}");

        let mut doc = document();
        let message = doc_edit::set_path(
            &mut doc,
            &steps("providers[9].name"),
            json!("x"),
            Policy::Strict,
        )
        .unwrap_err();
        assert!(message.contains("列表下标越界"), "{message}");
        assert!(
            message.contains("列表下标越界：providers 只有 1 项"),
            "{message}"
        );
    }

    #[test]
    fn unset_path_removes_and_reports_absent() {
        let mut doc = document();
        assert!(doc_edit::unset_path(&mut doc, &steps("gateway.port"), Policy::Strict).unwrap());
        assert!(doc["gateway"].get("port").is_none());
        // 幂等：第二次就是"缺席"。
        assert!(!doc_edit::unset_path(&mut doc, &steps("gateway.port"), Policy::Strict).unwrap());
        assert!(!doc_edit::unset_path(&mut doc, &steps("nothing.here"), Policy::Strict).unwrap());
        // 整个列表元素也可移除。
        assert!(doc_edit::unset_path(&mut doc, &steps("providers[0]"), Policy::Strict).unwrap());
        assert_eq!(doc["providers"], json!([]));
    }

    #[test]
    fn remove_indexed_reports_bounds_with_length() {
        let mut doc = document();
        let removed = doc_edit::remove_indexed(&mut doc, &steps("tools[0]")).unwrap();
        assert_eq!(removed, json!("bash"));
        assert_eq!(doc["tools"], json!([]));
        let message = doc_edit::remove_indexed(&mut doc, &steps("tools[5]")).unwrap_err();
        assert!(message.contains("列表下标越界"), "{message}");
        assert!(message.contains("tools 只有 0 项"), "{message}");
    }

    #[test]
    fn move_indexed_moves_in_both_directions() {
        let mut doc = json!({"tools": ["a", "b", "c"]});
        let outcome =
            doc_edit::move_indexed(&mut doc, &steps("tools[2]"), -1, Policy::Strict).unwrap();
        assert_eq!(
            outcome,
            doc_edit::MoveOutcome {
                moved: true,
                from: 2,
                to: 1,
                length: 3
            }
        );
        assert_eq!(doc["tools"], json!(["a", "c", "b"]));
        let outcome =
            doc_edit::move_indexed(&mut doc, &steps("tools[0]"), 2, Policy::Strict).unwrap();
        assert_eq!(outcome.to, 2);
        assert_eq!(doc["tools"], json!(["c", "b", "a"]));
    }

    #[test]
    fn move_indexed_clamps_at_edges_and_tolerates_i64_extremes() {
        let mut doc = json!({"tools": ["a", "b"]});
        // 边界钳制：向上越界 = 原地不动（moved=false 由调用方短路）。
        let outcome =
            doc_edit::move_indexed(&mut doc, &steps("tools[0]"), -5, Policy::Strict).unwrap();
        assert!(!outcome.moved);
        assert_eq!(doc["tools"], json!(["a", "b"]));
        // 向下越界钳到末位。
        let outcome =
            doc_edit::move_indexed(&mut doc, &steps("tools[0]"), i64::MAX, Policy::Strict).unwrap();
        assert_eq!(outcome.to, 1);
        assert_eq!(doc["tools"], json!(["b", "a"]));
        // 负向极值也不能溢出。
        let outcome =
            doc_edit::move_indexed(&mut doc, &steps("tools[1]"), i64::MIN, Policy::Strict).unwrap();
        assert_eq!(outcome.to, 0);
        // delta = 0 → 无位移。
        let outcome =
            doc_edit::move_indexed(&mut doc, &steps("tools[1]"), 0, Policy::Strict).unwrap();
        assert!(!outcome.moved);
    }

    #[test]
    fn append_item_appends_and_materializes_declared_defaults() {
        let root = catalog();
        let providers = resolve_node(&root, &document(), &steps("providers")).unwrap();

        // 标量列表：文档里有 → 直接追加。
        let mut doc = document();
        let index = doc_edit::append_item(
            &mut doc,
            &steps("tools"),
            providers,
            json!("read"),
            Policy::Strict,
        )
        .unwrap();
        assert_eq!(index, 1);
        assert_eq!(doc["tools"], json!(["bash", "read"]));

        // 列表缺席 → 用声明默认值物化后追加（否则 "add 到默认值" 无处可落）。
        let mut doc = json!({});
        let tools_node = resolve_node(&root, &doc, &steps("tools")).unwrap();
        let index = doc_edit::append_item(
            &mut doc,
            &steps("tools"),
            tools_node,
            json!("read"),
            Policy::Strict,
        )
        .unwrap();
        assert_eq!(index, 1);
        assert_eq!(doc["tools"], json!(["bash", "read"]));

        // 没有默认值且缺席 → 从空列表开始。
        let mut doc = json!({});
        let agents_node = resolve_node(&root, &doc, &steps("agents")).unwrap();
        let index = doc_edit::append_item(
            &mut doc,
            &steps("agents"),
            agents_node,
            json!({"name": "a"}),
            Policy::Strict,
        )
        .unwrap();
        assert_eq!(index, 0);
        assert_eq!(doc["agents"], json!([{"name": "a"}]));

        // 文档里的值不是数组 → 用法错误。
        let mut doc = json!({"tools": "oops"});
        let message = doc_edit::append_item(
            &mut doc,
            &steps("tools"),
            tools_node,
            json!("x"),
            Policy::Strict,
        )
        .unwrap_err();
        assert!(message.contains("不是列表"), "{message}");
    }

    // ------------------------------------------------------------
    // plan_write
    // ------------------------------------------------------------

    fn plan(op: &WriteOp) -> Result<Plan, String> {
        plan_write(op, &catalog(), &document())
    }

    fn saved_document(plan: Result<Plan, String>) -> Value {
        match plan.expect("plan must succeed") {
            Plan::Save { document, .. } => document,
            Plan::Noop { message, .. } => panic!("expected a save, got noop: {message}"),
        }
    }

    #[test]
    fn plan_set_coerces_by_catalog_kind() {
        let doc = saved_document(plan(&WriteOp::Set {
            path: "images.max_bytes".to_string(),
            value: Some("1000000".to_string()),
            json_value: None,
        }));
        assert_eq!(doc["images"]["max_bytes"], json!(1000000));

        let doc = saved_document(plan(&WriteOp::Set {
            path: "providers[0].models[0]".to_string(),
            value: Some("ds-pro".to_string()),
            json_value: None,
        }));
        assert_eq!(doc["providers"][0]["models"][0], json!("ds-pro"));
    }

    #[test]
    fn plan_set_replaces_a_union_element_in_the_new_values_shape() {
        // 对象形态 → 简单形态：裸字符串就地替换（不用 --json-value 也能表达）。
        let doc = saved_document(plan(&WriteOp::Set {
            path: "providers[0].models[1]".to_string(),
            value: Some("ds-pro-lite".to_string()),
            json_value: None,
        }));
        assert_eq!(doc["providers"][0]["models"][1], json!("ds-pro-lite"));

        // 简单形态 → 对象形态：JSON 对象就地替换。
        let doc = saved_document(plan(&WriteOp::Set {
            path: "providers[0].models[0]".to_string(),
            value: Some(r#"{"id":"ds-flash-2"}"#.to_string()),
            json_value: None,
        }));
        assert_eq!(
            doc["providers"][0]["models"][0],
            json!({"id": "ds-flash-2"})
        );

        // --json-value 同样按新值判形态。
        let doc = saved_document(plan(&WriteOp::Set {
            path: "providers[0].models[1]".to_string(),
            value: None,
            json_value: Some("\"ds-plain\"".to_string()),
        }));
        assert_eq!(doc["providers"][0]["models"][1], json!("ds-plain"));
    }

    #[test]
    fn plan_set_missing_value_is_usage_error() {
        let message = plan(&WriteOp::Set {
            path: "gateway.port".to_string(),
            value: None,
            json_value: None,
        })
        .unwrap_err();
        assert!(message.contains("缺少 <value>"), "{message}");
    }

    #[test]
    fn plan_set_json_value_bypasses_kind_checks() {
        // 故意给 int 字段一个字符串：本地不拦（后端才是校验器）。
        let doc = saved_document(plan(&WriteOp::Set {
            path: "gateway.port".to_string(),
            value: None,
            json_value: Some("\"not-a-number\"".to_string()),
        }));
        assert_eq!(doc["gateway"]["port"], json!("not-a-number"));
        // 非法 JSON → 用法错误。
        let message = plan(&WriteOp::Set {
            path: "gateway.port".to_string(),
            value: None,
            json_value: Some("{oops".to_string()),
        })
        .unwrap_err();
        assert!(message.starts_with("不是合法 JSON"), "{message}");
        // 位置参数与 --json-value 互斥。
        let message = plan(&WriteOp::Set {
            path: "gateway.port".to_string(),
            value: Some("1".to_string()),
            json_value: Some("2".to_string()),
        })
        .unwrap_err();
        assert!(message.contains("不能同时给出"), "{message}");
    }

    #[test]
    fn plan_write_rejects_template_empty_and_unknown_paths() {
        for path in ["providers[]", "providers[].api_key"] {
            let message = plan(&WriteOp::Unset {
                path: path.to_string(),
            })
            .unwrap_err();
            assert!(message.contains("模板路径"), "{path}: {message}");
        }
        let message = plan(&WriteOp::Unset {
            path: String::new(),
        })
        .unwrap_err();
        assert!(message.contains("路径不能为空"), "{message}");
        let message = plan(&WriteOp::Unset {
            path: "nope.nope".to_string(),
        })
        .unwrap_err();
        assert!(message.contains("路径不在设置目录中"), "{message}");
        // 自由 map 的内部不可寻址（catalog 只声明到 map 这一层）。
        let message = plan(&WriteOp::Unset {
            path: "providers[0].extra_body.thinking".to_string(),
        })
        .unwrap_err();
        assert!(message.contains("路径不在设置目录中"), "{message}");
    }

    #[test]
    fn plan_unset_absent_is_a_noop() {
        let outcome = plan(&WriteOp::Unset {
            path: "gateway.host".to_string(),
        })
        .expect("absent key is not an error");
        match outcome {
            Plan::Noop { reason, message } => {
                assert_eq!(reason, "already_default");
                assert!(message.contains("gateway.host"), "{message}");
            }
            Plan::Save { .. } => panic!("unsetting an absent key must not save"),
        }
    }

    #[test]
    fn plan_move_zero_delta_and_edge_are_noops() {
        for (path, delta, reason) in [
            ("providers[0]", 0, "delta_zero"),
            ("providers[0]", -1, "already_at_edge"),
        ] {
            match plan(&WriteOp::Move {
                path: path.to_string(),
                delta,
            })
            .expect("no-op is fine")
            {
                Plan::Noop {
                    reason: got,
                    message,
                } => {
                    assert_eq!(got, reason, "{message}");
                }
                Plan::Save { .. } => panic!("expected a noop for ({path}, {delta})"),
            }
        }
        // 有意义的一次移动会产出保存。
        let doc = saved_document(plan(&WriteOp::Move {
            path: "providers[0].models[1]".to_string(),
            delta: -1,
        }));
        assert_eq!(
            doc["providers"][0]["models"],
            json!([{"id": "ds-pro"}, "ds-flash"])
        );
    }

    #[test]
    fn plan_move_requires_an_explicit_index() {
        let message = plan(&WriteOp::Move {
            path: "providers[0].models".to_string(),
            delta: 1,
        })
        .unwrap_err();
        assert!(message.contains("必须以具体下标结尾"), "{message}");
    }

    #[test]
    fn plan_add_requires_a_value_for_scalar_elements() {
        let message = plan(&WriteOp::Add {
            path: "tools".to_string(),
            value: None,
            json_value: None,
            variant: None,
        })
        .unwrap_err();
        assert!(message.contains("必须给 <value>"), "{message}");

        let doc = saved_document(plan(&WriteOp::Add {
            path: "tools".to_string(),
            value: Some("read".to_string()),
            json_value: None,
            variant: None,
        }));
        assert_eq!(doc["tools"], json!(["bash", "read"]));
    }

    #[test]
    fn plan_add_object_stub_and_variant_choice() {
        // 对象列表（单一 element）：缺省插 {}。
        let doc = saved_document(plan(&WriteOp::Add {
            path: "agents".to_string(),
            value: None,
            json_value: None,
            variant: None,
        }));
        assert_eq!(doc["agents"][1], json!({}));

        // variants 列表：缺省 simple → 裸字符串，必须给值。
        let message = plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: None,
            json_value: None,
            variant: None,
        })
        .unwrap_err();
        assert!(message.contains("必须给 <value>"), "{message}");

        // --variant simple + 值 → 追加裸字符串。
        let doc = saved_document(plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: Some("ds-max".to_string()),
            json_value: None,
            variant: Some("simple".to_string()),
        }));
        assert_eq!(doc["providers"][0]["models"][2], json!("ds-max"));

        // --variant object → 插 {} 骨架。
        let doc = saved_document(plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: None,
            json_value: None,
            variant: Some("object".to_string()),
        }));
        assert_eq!(doc["providers"][0]["models"][2], json!({}));

        // 对象元素也能一次给全（JSON）。
        let doc = saved_document(plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: Some(r#"{"id":"ds-pro-2"}"#.to_string()),
            json_value: None,
            variant: Some("object".to_string()),
        }));
        assert_eq!(doc["providers"][0]["models"][2], json!({"id": "ds-pro-2"}));

        // 非法 variant 词 / 不适用 variant → 用法错误。
        let message = plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: Some("x".to_string()),
            json_value: None,
            variant: Some("rich".to_string()),
        })
        .unwrap_err();
        assert!(
            message.contains("--variant 只接受 simple|object"),
            "{message}"
        );
        // 没有该形态的列表（tools 只有 simple）→ 用法错误。
        let message = plan(&WriteOp::Add {
            path: "tools".to_string(),
            value: Some("x".to_string()),
            json_value: None,
            variant: Some("object".to_string()),
        })
        .unwrap_err();
        assert!(message.contains("没有 object 形态"), "{message}");

        let message = plan(&WriteOp::Add {
            path: "providers[0].protocol".to_string(),
            value: Some("x".to_string()),
            json_value: None,
            variant: None,
        })
        .unwrap_err();
        assert!(message.contains("不是列表"), "{message}");
    }

    #[test]
    fn plan_add_derives_the_new_item_path_in_the_note() {
        match plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: Some("ds-max".to_string()),
            json_value: None,
            variant: Some("simple".to_string()),
        })
        .unwrap()
        {
            Plan::Save { note, .. } => {
                assert_eq!(note.as_deref(), Some("新增项：providers[0].models[2]"));
            }
            Plan::Noop { .. } => panic!("expected a save"),
        }
    }

    #[test]
    fn plan_remove_redacts_secrets_in_the_note() {
        // 对抗性输入：文档里的 api_key 是明文（后端本该掩码，这里验证纵深防御）。
        let mut values = document();
        values["providers"][0]["api_key"] = json!("sk-live-plaintext-0001");
        let plan = plan_write(
            &WriteOp::Remove {
                path: "providers[0]".to_string(),
            },
            &catalog(),
            &values,
        )
        .expect("remove plans");
        match plan {
            Plan::Save { note, .. } => {
                let note = note.expect("note");
                assert!(
                    !note.contains("sk-live-plaintext-0001"),
                    "摘要不能带出密文：{note}"
                );
                // 摘要会被截断，所以单独证明"脱敏真的发生"（而不是只有截断在挡）。
                let root = catalog();
                let element =
                    resolve_node(&root, &values, &steps("providers[0]")).expect("element");
                let redacted = redact_secrets(element, &values["providers"][0]);
                assert_eq!(redacted["api_key"], Value::Null);
                assert_eq!(redacted["name"], json!("qoder"));
            }
            Plan::Noop { .. } => panic!("expected a save"),
        }
    }
    #[test]
    fn render_write_text_ok_includes_changed_restart_reload_and_backup() {
        let response = SettingsSetResponse {
            warnings: Vec::new(),
            ok: true,
            fingerprint: "fp-2".to_string(),
            problems: Vec::new(),
            changed: vec![
                "gateway.port".to_string(),
                "providers[0].base_url".to_string(),
            ],
            restart_required: vec!["gateway.port".to_string()],
            reload: Some(ReloadResponse {
                ok: false,
                results: vec![
                    ReloadResultItem {
                        name: "config.yaml".to_string(),
                        ok: true,
                        detail: None,
                    },
                    ReloadResultItem {
                        name: "hooks".to_string(),
                        ok: false,
                        detail: Some("boom".to_string()),
                    },
                ],
            }),
            setup_mode_exited: true,
            backup_path: Some("/tmp/config.yaml.bak".to_string()),
        };
        let text = render_write_text(&response, Some("新增项：providers[1]"));
        assert!(text.contains("✓ 已保存（2 处变更）"), "{text}");
        assert!(text.contains("  · gateway.port"), "{text}");
        assert!(text.contains("新增项：providers[1]"), "{text}");
        assert!(text.contains("⚠ 需重启网关才生效：gateway.port"), "{text}");
        assert!(text.contains("热重载：config.yaml ✓ · hooks ✗"), "{text}");
        assert!(text.contains("  ✗ hooks：boom"), "{text}");
        assert!(text.contains("⚠ 热重载未全部成功"), "{text}");
        assert!(text.contains("✓ 网关已转入正常模式"), "{text}");
        assert!(text.contains("备份：/tmp/config.yaml.bak"), "{text}");
    }
    #[test]
    fn render_write_text_refused_lists_problems_and_never_writes() {
        let response = SettingsSetResponse {
            warnings: Vec::new(),
            ok: false,
            fingerprint: "fp-1".to_string(),
            problems: vec![problem(
                Some("providers[0].models"),
                "empty_list",
                "至少声明一个模型",
                Some("加一个模型"),
            )],
            changed: Vec::new(),
            restart_required: Vec::new(),
            reload: None,
            setup_mode_exited: false,
            backup_path: None,
        };
        let text = render_write_text(&response, None);
        assert!(text.contains("✗ 保存被拒绝"), "{text}");
        assert!(
            text.contains("providers[0].models: 至少声明一个模型"),
            "{text}"
        );
        assert!(text.contains("↳ 加一个模型"), "{text}");
        assert!(text.contains("共 1 个问题"), "{text}");
    }
}
