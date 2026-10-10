//! 稀疏配置文档的**路径代数与编辑原语 —— 唯一实现**。
//!
//! 同一件事（按规范路径读 / 写 / 删 / 列表增删移，按目录声明造 stub，下标重排）有两个
//! 使用者，它们的 UX 语义**不同**，但差异只在策略上，没有理由留两份实现——两处各自重写
//! 同一条规则就是漂移的开始（同 [`super::pinning`] 的取舍）：
//!
//! | 使用者 | 缺的键 / 形状不符 / 下标越界 |
//! |---|---|
//! | `wing config`（[`Policy::Strict`]）| 用法错误（中文文案，exit 4）：拒绝，绝不扩张文档 |
//! | TUI 设置面板（[`Policy::Lenient`]）| 「这一步没做成」：按需创建容器、清理残渣、不报错 |
//!
//! 差异是**显式**的，分两类，没有第三类：
//!
//! 1. **文档该被改成什么样**（写操作真正的语义差）→ [`Policy`] 是这些入口的必填参数：
//!    中间容器建不建（[`set_path`]）、空容器残渣清不清（[`unset_path`]）、越界钳不钳制
//!    （[`move_indexed`]）、声明默认值物不物化（[`append_item`]）、新项初值写多详细
//!    （[`empty_value`]）；
//! 2. **同一份判定怎么读**（面板把用法错误读成「没反应」）→ 调用方 `.ok()` / `.is_ok()`：
//!    形状与下标的判定、连同文案，两种 UX 只有一份 —— [`remove_indexed`] 因此连策略参数
//!    都不需要，[`move_indexed`] 的形状判定也与策略无关（策略只决定越界口径）。
//!
//! 逐入口的策略矩阵：
//!
//! | 入口 | [`Policy::Strict`]（CLI） | [`Policy::Lenient`]（面板） |
//! |---|---|---|
//! | [`set_path`] | 中间层不是 object / array、数组下标越界 → 用法错误 | 形状不符就地换成正确的容器、数组补 `null` |
//! | [`unset_path`] | 幂等（缺席 → `false`）；只动点名的那条路径 | 额外逐级清理变空的 object / 数组容器（不留残渣） |
//! | [`move_indexed`] | 目标下标**钳制**到 `[0, len-1]`（位移 0 → `moved == false`） | 越界 = `moved == false`（不钳制；面板只给 ±1，两种口径在 ±1 上重合） |
//! | [`append_item`] | 列表缺席 / 为 `null` 时用目录的**声明默认值**物化 | 从不物化声明默认值（协议不携带 `list` 的默认值，design A2），缺席 = 空列表 |
//! | [`empty_value`] | 骨架不发明假值：`{}` / `[]`，标量 → `None` | 标量给 kind 的空值，`object` 写**必填**字段 |
//!
//! 路径原语（[`format_path`] / [`join_path`] / [`index_path`] / [`ancestors`] /
//! [`path_is_within`] / [`get_path`]）与下标重排（[`remap_index`] / [`remove_index_map`] /
//! [`swap_index_map`]）没有策略：两种语义下它们是同一件事。
//!
//! 路径文法在 design.md §5.2 冻结，解析器是 [`parse_path`]（三变体 Key / Index / Element，
//! 下标不查界）。本模块只加「用」它的原语，**没有第二份解析**；编辑入口一律收
//! `&[PathStep]`（已解析的规范路径），字符串入口只留给本来就按字符串存索引的
//! [`ancestors`] / [`remap_index`]。

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use wing_api_client::models::PathStep;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;
use wing_api_client::models::parse_path;

/// 两个使用者的 UX 策略 —— 每个入口的必填参数（语义差异显式，见模块文档的矩阵）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// 严格：`wing config` 的用法错误语义。`Err` 的文案就是 CLI 的文案（逐字）。
    Strict,
    /// 宽容：TUI 设置面板的交互语义。形状问题不是错误，是「这一步没做成」——
    /// 面板没有报错出口（用户手里只有一棵树和一串按键）。
    Lenient,
}

/// 模板步（`[]`）出现在**文档路径**里的拒绝理由（`catalog` 路径才用它）。
const TEMPLATE_STEP: &str = "模板路径（[]）不能用于写操作";

// ============================================================
// 路径代数
// ============================================================

/// 规范路径 → 文本（`Key` 用 `.` 连接，`Index` / `Element` 用方括号）。
pub fn format_path(steps: &[PathStep]) -> String {
    let mut out = String::new();
    for step in steps {
        match step {
            PathStep::Key(name) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(name);
            }
            PathStep::Index(index) => {
                out.push('[');
                out.push_str(&index.to_string());
                out.push(']');
            }
            PathStep::Element => out.push_str("[]"),
        }
    }
    out
}

/// 按规范路径取文档里的值（`None` = 路径缺席或含模板步；下标不查界，
/// 越界同样只是取不到 —— 见 P3 的裁定）。
pub fn get_path<'a>(doc: &'a Value, steps: &[PathStep]) -> Option<&'a Value> {
    let mut current = doc;
    for step in steps {
        current = match step {
            PathStep::Key(name) => current.get(name)?,
            PathStep::Index(index) => current.get(*index)?,
            PathStep::Element => return None,
        };
    }
    Some(current)
}

/// 把子段拼到父路径上（空父路径 → 子段本身）。
pub fn join_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        segment.to_string()
    } else {
        format!("{parent}.{segment}")
    }
}

/// 数组下标拼到列表路径上：`providers` + 0 → `providers[0]`。
pub fn index_path(list_path: &str, index: usize) -> String {
    format!("{list_path}[{index}]")
}

/// 某个具体路径的**全部祖先前缀**（不含自己，含根的空路径）——
/// `providers[0].api_key` → `["", "providers", "providers[0]"]`。
pub fn ancestors(path: &str) -> Vec<String> {
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
pub fn path_is_within(outer: &str, inner: &str) -> bool {
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
pub fn remap_index(
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
pub fn remove_index_map(removed: usize) -> impl Fn(usize) -> Option<usize> {
    move |i| match i.cmp(&removed) {
        std::cmp::Ordering::Less => Some(i),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(i - 1),
    }
}

/// 交换两个下标的映射（列表项上移 / 下移）。
pub fn swap_index_map(a: usize, b: usize) -> impl Fn(usize) -> Option<usize> {
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

// ============================================================
// 文档编辑
// ============================================================

/// 沿路径写值（`set`）。
///
/// - [`Policy::Strict`]（CLI）：中间缺失 / 为 `null` 的 object 自动创建；中间层是标量、
///   或数组下标越界 → 用法错误（不猜、**不扩张列表** —— 扩张用 `add`）；模板步 → 用法错误
///   （错误路径上可能已经物化了沿途的中间 object —— 那份文档在 CLI 里不会保存）。
/// - [`Policy::Lenient`]（面板）：中间容器按需创建（形状不符就地换成正确的那种容器，
///   数组按需补 `null`）；模板步 = 到此为止（不写值，走过的中间步照旧；不是错误）。
pub fn set_path(
    doc: &mut Value,
    steps: &[PathStep],
    new_value: Value,
    policy: Policy,
) -> Result<(), String> {
    set_path_at(doc, steps, 0, new_value, policy)
}

fn set_path_at(
    current: &mut Value,
    steps: &[PathStep],
    at: usize,
    new_value: Value,
    policy: Policy,
) -> Result<(), String> {
    if at == steps.len() {
        *current = new_value;
        return Ok(());
    }
    match &steps[at] {
        PathStep::Key(name) => {
            if !current.is_object() {
                match policy {
                    Policy::Strict => {
                        return Err(format!(
                            "路径 {} 的中间层 {} 不是对象",
                            format_path(steps),
                            format_path(&steps[..at])
                        ));
                    }
                    Policy::Lenient => *current = Value::Object(Map::new()),
                }
            }
            let map = current.as_object_mut().expect("shaped just above");
            let next = map.entry(name.clone()).or_insert(Value::Null);
            if at + 1 == steps.len() {
                *next = new_value;
                return Ok(());
            }
            // 严格策略在下探**之前**就把 null 物化成 object（它的下一层不做形状修复）；
            // 宽容策略把这一步留给下一层就地修（于是「模板步停在半路」时留下的中间值和
            // 原实现逐字一致）。
            if next.is_null() && policy == Policy::Strict {
                *next = Value::Object(Map::new());
            }
            set_path_at(next, steps, at + 1, new_value, policy)
        }
        PathStep::Index(index) => {
            if !current.is_array() {
                match policy {
                    Policy::Strict => {
                        return Err(format!(
                            "路径 {} 的中间层 {} 不是列表",
                            format_path(steps),
                            format_path(&steps[..at])
                        ));
                    }
                    Policy::Lenient => *current = Value::Array(Vec::new()),
                }
            }
            let array = current.as_array_mut().expect("shaped just above");
            if *index >= array.len() {
                match policy {
                    Policy::Strict => {
                        return Err(format!(
                            "列表下标越界：{} 只有 {} 项（下标 {}）",
                            format_path(&steps[..at]),
                            array.len(),
                            index
                        ));
                    }
                    Policy::Lenient => {
                        while array.len() <= *index {
                            array.push(Value::Null);
                        }
                    }
                }
            }
            set_path_at(&mut array[*index], steps, at + 1, new_value, policy)
        }
        PathStep::Element => match policy {
            Policy::Strict => Err(TEMPLATE_STEP.to_string()),
            Policy::Lenient => Ok(()),
        },
    }
}

/// 从稀疏文档移除一个路径（「回到跟随默认」）；`Ok(true)` = 确实移除了这个键。
///
/// - [`Policy::Strict`]（CLI `unset`）：中间层形状不符同样视作「缺席」（幂等无操作，不是
///   错误）；**只动用户点名的那条路径**，别的键一个不碰；模板步 → 用法错误。
/// - [`Policy::Lenient`]（面板复位）：额外逐级清理因此变空的 object / 数组祖先
///   （不让 `{"gateway":{}}` 这样的残渣留在文件里）；模板步 / 空路径 = 没做成（`false`）。
pub fn unset_path(doc: &mut Value, steps: &[PathStep], policy: Policy) -> Result<bool, String> {
    let Some((first, rest)) = steps.split_first() else {
        return match policy {
            Policy::Strict => Err("路径不能为空".to_string()),
            Policy::Lenient => Ok(false),
        };
    };
    match first {
        PathStep::Key(name) => {
            let Some(object) = doc.as_object_mut() else {
                return Ok(false);
            };
            if rest.is_empty() {
                return Ok(object.remove(name).is_some());
            }
            let removed = match object.get_mut(name) {
                Some(child) => unset_path(child, rest, policy)?,
                None => return Ok(false),
            };
            if removed
                && policy == Policy::Lenient
                && object.get(name).is_some_and(is_empty_container)
            {
                object.remove(name);
            }
            Ok(removed)
        }
        PathStep::Index(index) => {
            let Some(array) = doc.as_array_mut() else {
                return Ok(false);
            };
            if rest.is_empty() {
                if *index < array.len() {
                    array.remove(*index);
                    return Ok(true);
                }
                return Ok(false);
            }
            match array.get_mut(*index) {
                Some(child) => unset_path(child, rest, policy),
                None => Ok(false),
            }
        }
        PathStep::Element => match policy {
            Policy::Strict => Err(TEMPLATE_STEP.to_string()),
            Policy::Lenient => Ok(false),
        },
    }
}

/// 空 object / 空数组（清理残渣时的判定）。
fn is_empty_container(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.is_empty(),
        Value::Array(array) => array.is_empty(),
        _ => false,
    }
}

/// 删掉「列表 + 具体下标」处的元素，返回它（`remove`）。
///
/// 这里的判定与文案**两种 UX 共用**：路径必须以具体下标结尾、父层必须存在且是数组、
/// 下标必须在界内，任一不符都是这句用法错误。CLI 原样上报（exit 4），面板把它读成
/// 「没反应」（`.is_ok()` → 无操作）—— 差别只在怎么读，所以没有策略参数。
pub fn remove_indexed(doc: &mut Value, steps: &[PathStep]) -> Result<Value, String> {
    let (array, index) = list_slot(doc, steps)?;
    Ok(array.remove(index))
}

/// `move` 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveOutcome {
    /// 是否真的发生了位移（`false` = 调用方短路，不写盘）。
    pub moved: bool,
    /// 位移前的下标。
    pub from: usize,
    /// 位移后的下标（`moved == false` 时与 `from` 相同）。
    pub to: usize,
    /// 列表长度（写回 "第 n / m 项" 这类回执用）。
    pub length: usize,
}

impl MoveOutcome {
    /// 没位移（原地 / 越界不移动）：下标不动，长度照报。
    fn unmoved(index: usize, length: usize) -> Self {
        Self {
            moved: false,
            from: index,
            to: index,
            length,
        }
    }
}

/// 把「列表 + 具体下标」处的元素挪到 `[index + delta]`（`move`）。
///
/// 形状判定与 [`remove_indexed`] 同一条（用法错误 / 面板读成「没反应」）；**越界**的口径
/// 由策略决定，见模块文档的矩阵：严格策略钳制到边界，宽容策略不移动。
pub fn move_indexed(
    doc: &mut Value,
    steps: &[PathStep],
    delta: i64,
    policy: Policy,
) -> Result<MoveOutcome, String> {
    let (array, index) = list_slot(doc, steps)?;
    let length = array.len();
    // i128：`delta` 取 i64 极值时 `index + delta` 也不能溢出。
    let target = index as i128 + delta as i128;
    let target = match policy {
        Policy::Strict => target.clamp(0, length as i128 - 1) as usize,
        Policy::Lenient => {
            if !(0..length as i128).contains(&target) {
                return Ok(MoveOutcome::unmoved(index, length));
            }
            target as usize
        }
    };
    if target == index {
        return Ok(MoveOutcome::unmoved(index, length));
    }
    let item = array.remove(index);
    array.insert(target, item);
    Ok(MoveOutcome {
        moved: true,
        from: index,
        to: target,
        length,
    })
}

/// 在列表末尾追加一项，返回新下标（`add`）。
///
/// - [`Policy::Strict`]（CLI）：列表在稀疏文档里缺席 / 为 `null` 时，用目录的**声明默认值**
///   物化（没有默认则空列表）—— 否则 `add tools bash` 会在「没显式写过 tools」时无处可落；
///   文档里的值不是数组 → 用法错误。
/// - [`Policy::Lenient`]（面板）：从不物化声明默认值（见模块文档），缺席 / 形状不符都从
///   空列表开始。
pub fn append_item(
    doc: &mut Value,
    steps: &[PathStep],
    list: &SettingNode,
    item: Value,
    policy: Policy,
) -> Result<usize, String> {
    let mut array = match (get_path(doc, steps), policy) {
        (Some(Value::Array(items)), _) => items.clone(),
        (Some(Value::Null) | None, Policy::Strict) => match (&list.default, list.has_default) {
            (Some(Value::Array(declared)), true) => declared.clone(),
            _ => Vec::new(),
        },
        (Some(_), Policy::Strict) => {
            return Err(format!(
                "{} 不是列表（文档里的值不是 JSON 数组）",
                format_path(steps)
            ));
        }
        (Some(_) | None, Policy::Lenient) => Vec::new(),
    };
    let index = array.len();
    array.push(item);
    // 落位走同一个 [`set_path`]：严格策略下若中间层形状不符会照实报错，宽容策略就地修。
    set_path(doc, steps, Value::Array(array), policy)?;
    Ok(index)
}

/// 新增项的初值（目录驱动的骨架）。
///
/// - [`Policy::Strict`]（CLI `add` 没给 `<value>` 时）：骨架**不发明假值**（不填 name /
///   base_url 之类）—— `object` / `map` → `{}`、`list` → `[]`、标量 → `None`（调用方报
///   「必须给 <value>」）。插进去的 problems 由后端如实报告，用户按提示补齐。
/// - [`Policy::Lenient`]（面板新增一项）：每一项都要有可编辑的初值 —— 标量给 kind 的空值
///   （`""` / `0` / `false` / 枚举首项），`object` / `map` 只写**必填**字段（缺席即默认）。
pub fn empty_value(node: &SettingNode, policy: Policy) -> Option<Value> {
    match node.kind {
        SettingKind::Object | SettingKind::Map => Some(stub_object(node, policy)),
        SettingKind::List => Some(json!([])),
        // 严格策略下标量必须由用户给值（`None` = 调用方报用法错误）。
        _ if policy == Policy::Strict => None,
        SettingKind::Bool => Some(json!(false)),
        SettingKind::Int => Some(json!(0)),
        SettingKind::Float => Some(json!(0.0)),
        SettingKind::Enum => Some(
            node.choices
                .first()
                .map_or_else(|| json!(""), |choice| json!(choice.value)),
        ),
        SettingKind::Str | SettingKind::Secret | SettingKind::Unknown(_) => Some(json!("")),
    }
}

/// 新对象的 stub：严格策略写**空**对象（不发明假值），宽容策略写**必填**字段的初值
/// （递归：必填的 `object` / `list` 字段也要成型）。
fn stub_object(node: &SettingNode, policy: Policy) -> Value {
    let mut map = Map::new();
    if policy == Policy::Lenient {
        for child in node.children.iter().filter(|child| child.required) {
            if let Some(value) = empty_value(child, policy) {
                map.insert(child.key.clone(), value);
            }
        }
    }
    Value::Object(map)
}

/// 定位「列表 + 具体下标」：路径必须以 `Index` 结尾、父层必须存在、父层必须是数组、
/// 下标必须在界内。判定与文案只有这一份（两种 UX 按各自的口径读它的 `Err`）。
fn list_slot<'a>(
    doc: &'a mut Value,
    steps: &[PathStep],
) -> Result<(&'a mut Vec<Value>, usize), String> {
    let Some((PathStep::Index(index), parents)) = steps.split_last() else {
        return Err(format!(
            "{} 必须以具体下标结尾（如 providers[1]）",
            format_path(steps)
        ));
    };
    let mut current = doc;
    for step in parents {
        let next = match step {
            PathStep::Key(name) => current.get_mut(name),
            PathStep::Index(inner) => current.get_mut(*inner),
            PathStep::Element => return Err(TEMPLATE_STEP.to_string()),
        };
        let Some(next) = next else {
            return Err(format!(
                "路径不在当前文档中：{}（先 set / add 建出来）",
                format_path(parents)
            ));
        };
        current = next;
    }
    let parent = format_path(parents);
    let Some(array) = current.as_array_mut() else {
        return Err(format!("{parent} 不是列表（文档里的值不是 JSON 数组）"));
    };
    if *index >= array.len() {
        let length = array.len();
        return Err(format!(
            "列表下标越界：{parent} 只有 {length} 项（下标 {index}）"
        ));
    }
    Ok((array, *index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::SettingChoice;

    fn steps(path: &str) -> Vec<PathStep> {
        parse_path(path).unwrap_or_else(|| panic!("测试路径必须合法：{path}"))
    }

    /// 目录节点夹具：只写用例读得到的字段，其余走声明的缺省。
    fn node(key: &str, kind: SettingKind) -> SettingNode {
        serde_json::from_value(json!({
            "key": key,
            "path": key,
            "title": key,
            "doc": format!("{key} 的说明"),
            "kind": kind,
        }))
        .expect("夹具必须能反序列化")
    }

    fn object(key: &str, children: Vec<SettingNode>) -> SettingNode {
        let mut node = node(key, SettingKind::Object);
        node.children = children;
        node
    }

    fn list(key: &str, element: SettingNode) -> SettingNode {
        let mut node = node(key, SettingKind::List);
        node.element = Some(Box::new(element));
        node
    }

    /// 声明了默认值的列表（`add` 的物化路径）。
    fn declared_list(key: &str, element: SettingNode, default: Value) -> SettingNode {
        let mut node = list(key, element);
        node.default = Some(default);
        node.has_default = true;
        node
    }

    fn required_str(key: &str) -> SettingNode {
        let mut node = node(key, SettingKind::Str);
        node.required = true;
        node
    }

    // ── 路径原语（两种 UX 共用，没有策略） ────────────────────

    #[test]
    fn get_path_reads_objects_and_arrays_and_stops_at_template_steps() {
        let doc = json!({"providers": [{"name": "a"}]});
        assert_eq!(
            get_path(&doc, &steps("providers[0].name")),
            Some(&json!("a"))
        );
        assert_eq!(get_path(&doc, &[]), Some(&doc), "空路径 = 文档自己");
        assert_eq!(
            get_path(&doc, &steps("providers[7].name")),
            None,
            "下标不查界，取不到就是 None"
        );
        assert_eq!(
            get_path(&doc, &steps("providers[]")),
            None,
            "模板步不是文档路径"
        );
    }

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

    // ── 写：策略决定文档变成什么样 ───────────────────────────

    #[test]
    fn set_path_creates_missing_intermediate_objects_in_both_policies() {
        for policy in [Policy::Strict, Policy::Lenient] {
            let mut doc = json!({});
            set_path(
                &mut doc,
                &steps("gateway.auth.enabled"),
                json!(true),
                policy,
            )
            .unwrap();
            assert_eq!(doc, json!({"gateway": {"auth": {"enabled": true}}}));
            // null 中间层同样被物化成对象（nullable object 字段）。
            let mut doc = json!({"gateway": {"auth": null}});
            set_path(
                &mut doc,
                &steps("gateway.auth.enabled"),
                json!(true),
                policy,
            )
            .unwrap();
            assert_eq!(doc, json!({"gateway": {"auth": {"enabled": true}}}));
        }
    }

    #[test]
    fn set_path_strict_refuses_the_shapes_lenient_replaces() {
        // 中间层是标量：严格 → 用法错误（一个字节都不改）；宽容 → 就地换成对象。
        let mut strict = json!({"gateway": {"port": 8080}});
        assert!(
            set_path(
                &mut strict,
                &steps("gateway.port.sub"),
                json!(1),
                Policy::Strict
            )
            .is_err()
        );
        assert_eq!(strict, json!({"gateway": {"port": 8080}}));
        let mut lenient = json!({"gateway": {"port": 8080}});
        set_path(
            &mut lenient,
            &steps("gateway.port.sub"),
            json!(1),
            Policy::Lenient,
        )
        .unwrap();
        assert_eq!(lenient, json!({"gateway": {"port": {"sub": 1}}}));

        // 中间层是标量而路径要走数组：同上。
        let mut strict = json!({"gateway": {"port": 8080}});
        assert!(
            set_path(
                &mut strict,
                &steps("gateway.port[0]"),
                json!(1),
                Policy::Strict
            )
            .is_err()
        );
        let mut lenient = json!({"gateway": {"port": 8080}});
        set_path(
            &mut lenient,
            &steps("gateway.port[0]"),
            json!(1),
            Policy::Lenient,
        )
        .unwrap();
        assert_eq!(lenient, json!({"gateway": {"port": [1]}}));
    }

    #[test]
    fn set_path_strict_never_grows_arrays_while_lenient_pads_with_null() {
        let mut strict = json!({"tools": []});
        assert!(set_path(&mut strict, &steps("tools[1]"), json!("x"), Policy::Strict).is_err());
        assert_eq!(strict, json!({"tools": []}), "拒绝时不扩张列表");
        let mut lenient = json!({"tools": []});
        set_path(
            &mut lenient,
            &steps("tools[1]"),
            json!("x"),
            Policy::Lenient,
        )
        .unwrap();
        assert_eq!(lenient, json!({"tools": [null, "x"]}), "中间缺位补 null");
    }

    #[test]
    fn set_path_template_step_is_an_error_in_strict_and_a_noop_in_lenient() {
        // 严格策略：模板步是用法错误，一个值都不写。
        let mut strict = json!({"providers": [{"name": "a"}]});
        assert!(
            set_path(
                &mut strict,
                &steps("providers[].name"),
                json!("x"),
                Policy::Strict
            )
            .is_err()
        );
        assert_eq!(strict, json!({"providers": [{"name": "a"}]}));
        // 宽容策略走到模板步就停下（沿途照旧建键，值一个都不写）。
        let mut lenient = json!({});
        set_path(
            &mut lenient,
            &steps("providers[]"),
            json!("x"),
            Policy::Lenient,
        )
        .unwrap();
        assert_eq!(lenient, json!({"providers": null}), "只留下走过的中间步");
        let mut lenient = json!({"providers": [{"name": "a"}]});
        set_path(
            &mut lenient,
            &steps("providers[].name"),
            json!("x"),
            Policy::Lenient,
        )
        .unwrap();
        assert_eq!(
            lenient,
            json!({"providers": [{"name": "a"}]}),
            "已有的值一个都不动"
        );
    }

    // ── 删：空容器残渣归谁清 ─────────────────────────────────

    #[test]
    fn unset_path_prunes_empty_ancestors_only_in_lenient_mode() {
        let mut strict = json!({"gateway": {"port": 32523}});
        assert!(unset_path(&mut strict, &steps("gateway.port"), Policy::Strict).unwrap());
        assert_eq!(strict, json!({"gateway": {}}), "严格策略只动点名的那条路径");
        let mut lenient = json!({"gateway": {"port": 32523}});
        assert!(unset_path(&mut lenient, &steps("gateway.port"), Policy::Lenient).unwrap());
        assert_eq!(lenient, json!({}), "宽容策略清掉空 object 残渣");
    }

    #[test]
    fn unset_path_is_idempotent_and_reports_absent_as_false() {
        for policy in [Policy::Strict, Policy::Lenient] {
            let mut doc =
                json!({"gateway": {"port": 32523}, "providers": [{"name": "a"}, {"name": "b"}]});
            assert!(unset_path(&mut doc, &steps("gateway.port"), policy).unwrap());
            assert!(doc["gateway"].get("port").is_none());
            // 幂等：第二次就是「缺席」。
            assert!(!unset_path(&mut doc, &steps("gateway.port"), policy).unwrap());
            assert!(!unset_path(&mut doc, &steps("nothing.here"), policy).unwrap());
            // 整个列表元素也可移除（列表还有别的项时，列表照旧在）。
            assert!(unset_path(&mut doc, &steps("providers[0]"), policy).unwrap());
            assert_eq!(doc["providers"], json!([{"name": "b"}]));
        }
    }

    #[test]
    fn unset_path_drops_the_emptied_containers_only_in_lenient_mode() {
        for policy in [Policy::Strict, Policy::Lenient] {
            let mut doc = json!({"providers": [{"name": "a"}, {"name": "b"}]});
            assert!(unset_path(&mut doc, &steps("providers[0]"), policy).unwrap());
            assert_eq!(
                doc["providers"],
                json!([{"name": "b"}]),
                "列表非空：两种策略都留着"
            );
        }
        // 列表被清空：严格策略留下 `[]`（只动点名的路径），宽容策略连空容器一起清掉。
        let mut strict = json!({"providers": [{"name": "a"}]});
        assert!(unset_path(&mut strict, &steps("providers[0]"), Policy::Strict).unwrap());
        assert_eq!(strict, json!({"providers": []}));
        let mut lenient = json!({"providers": [{"name": "a"}]});
        assert!(unset_path(&mut lenient, &steps("providers[0]"), Policy::Lenient).unwrap());
        assert_eq!(lenient, json!({}));
    }

    #[test]
    fn unset_path_template_step_is_an_error_in_strict_and_a_noop_in_lenient() {
        let mut doc = json!({"providers": [{"name": "a"}]});
        assert!(
            unset_path(&mut doc, &steps("providers[]"), Policy::Strict).is_err(),
            "严格策略的模板步是用法错误"
        );
        assert!(
            !unset_path(&mut doc, &steps("providers[]"), Policy::Lenient).unwrap(),
            "宽容策略只是没做成"
        );
        assert_eq!(
            doc["providers"],
            json!([{"name": "a"}]),
            "两种情况都不动文档"
        );
        assert!(unset_path(&mut doc, &[], Policy::Strict).is_err(), "空路径");
    }

    // ── 列表增删移 ───────────────────────────────────────────

    #[test]
    fn remove_indexed_keeps_one_verdict_that_both_ux_read_their_own_way() {
        let mut doc = json!({"tools": ["bash", "read"]});
        assert_eq!(
            remove_indexed(&mut doc, &steps("tools[0]")).unwrap(),
            json!("bash")
        );
        assert_eq!(doc, json!({"tools": ["read"]}));
        // 两种 UX 的差别只在怎么读 `Err`：CLI 上报（exit 4），面板 `.is_ok()` = 无操作。
        let before = doc.clone();
        assert!(remove_indexed(&mut doc, &steps("tools[5]")).is_err());
        assert_eq!(doc, before, "拒绝的那一次不碰文档");
    }

    #[test]
    fn move_indexed_clamps_in_strict_mode_and_refuses_out_of_range_in_lenient_mode() {
        // ±1 是面板的整个行程：两种口径在这里重合（相邻两格）。
        let mut lenient = json!({"tools": ["a", "b", "c"]});
        let outcome = move_indexed(&mut lenient, &steps("tools[0]"), 1, Policy::Lenient).unwrap();
        assert_eq!((outcome.from, outcome.to, outcome.length), (0, 1, 3));
        assert_eq!(lenient["tools"], json!(["b", "a", "c"]));
        let mut strict = json!({"tools": ["a", "b", "c"]});
        let outcome = move_indexed(&mut strict, &steps("tools[0]"), 1, Policy::Strict).unwrap();
        assert_eq!(
            outcome,
            MoveOutcome {
                moved: true,
                from: 0,
                to: 1,
                length: 3
            }
        );
        assert_eq!(strict, lenient, "±1 上两种口径结果相同");

        // 越界（面板不会给这么大的 delta，这里用它把两种口径的差别逼出来）：
        // 严格策略钳到边界，宽容策略原地不动（面板据此不重排集合）。
        let mut strict = json!({"tools": ["a", "b"]});
        let outcome = move_indexed(&mut strict, &steps("tools[0]"), 9, Policy::Strict).unwrap();
        assert_eq!((outcome.moved, outcome.to), (true, 1));
        let mut lenient = json!({"tools": ["a", "b"]});
        let outcome = move_indexed(&mut lenient, &steps("tools[0]"), 9, Policy::Lenient).unwrap();
        assert_eq!((outcome.moved, outcome.from, outcome.to), (false, 0, 0));
        assert_eq!(lenient["tools"], json!(["a", "b"]), "越界不移动");

        // 位移 0 / 已到边界：两种策略都只是「没位移」（调用方短路，不写盘）。
        let outcome = move_indexed(&mut lenient, &steps("tools[0]"), 0, Policy::Lenient).unwrap();
        assert!(!outcome.moved);
        let outcome = move_indexed(&mut lenient, &steps("tools[0]"), -5, Policy::Lenient).unwrap();
        assert!(!outcome.moved);
    }

    #[test]
    fn append_item_materializes_declared_defaults_only_in_strict_mode() {
        let tools = declared_list("tools", node("[]", SettingKind::Str), json!(["bash"]));

        // 列表缺席：严格策略用声明默认值物化（否则 "add 到默认值" 无处可落）；
        // 宽容策略从空列表开始（协议不携带 list 的默认值，design A2）。
        let mut strict = json!({});
        let index = append_item(
            &mut strict,
            &steps("tools"),
            &tools,
            json!("read"),
            Policy::Strict,
        );
        assert_eq!(index.unwrap(), 1);
        assert_eq!(strict, json!({"tools": ["bash", "read"]}));
        let mut lenient = json!({});
        let index = append_item(
            &mut lenient,
            &steps("tools"),
            &tools,
            json!("read"),
            Policy::Lenient,
        );
        assert_eq!(index.unwrap(), 0);
        assert_eq!(lenient, json!({"tools": ["read"]}));

        // 值不是数组：宽容策略从空列表开始（严格策略是用法错误，见 `cmd/config.rs` 的用例）。
        let mut lenient = json!({"tools": "oops"});
        let index = append_item(
            &mut lenient,
            &steps("tools"),
            &tools,
            json!("x"),
            Policy::Lenient,
        );
        assert_eq!(index.unwrap(), 0);
        assert_eq!(lenient, json!({"tools": ["x"]}));
    }

    // ── 新增项的初值 ─────────────────────────────────────────

    #[test]
    fn empty_value_fills_scalars_only_in_lenient_mode() {
        assert_eq!(
            empty_value(&node("name", SettingKind::Str), Policy::Strict),
            None
        );
        assert_eq!(
            empty_value(&node("name", SettingKind::Str), Policy::Lenient),
            Some(json!(""))
        );
        assert_eq!(
            empty_value(&node("count", SettingKind::Int), Policy::Lenient),
            Some(json!(0))
        );
        assert_eq!(
            empty_value(&node("ratio", SettingKind::Float), Policy::Lenient),
            Some(json!(0.0))
        );
        assert_eq!(
            empty_value(&node("on", SettingKind::Bool), Policy::Lenient),
            Some(json!(false))
        );
        assert_eq!(
            empty_value(
                &node("extra", SettingKind::Unknown("future".into())),
                Policy::Lenient
            ),
            Some(json!("")),
            "未知 kind 给空串，不猜结构"
        );
        // 容器骨架两种策略都给（严格策略只给容器，标量由用户给值）。
        assert_eq!(
            empty_value(&node("blocks", SettingKind::Map), Policy::Strict),
            Some(json!({}))
        );
        assert_eq!(
            empty_value(&node("tools", SettingKind::List), Policy::Lenient),
            Some(json!([]))
        );
    }

    #[test]
    fn empty_value_uses_the_first_choice_for_enums_in_lenient_mode() {
        let mut protocol = node("protocol", SettingKind::Enum);
        protocol.choices = vec![
            SettingChoice {
                value: "openai".to_string(),
                doc: None,
            },
            SettingChoice {
                value: "anthropic".to_string(),
                doc: None,
            },
        ];
        assert_eq!(
            empty_value(&protocol, Policy::Lenient),
            Some(json!("openai")),
            "枚举从声明首项起步"
        );
        assert_eq!(empty_value(&protocol, Policy::Strict), None);
    }

    #[test]
    fn empty_value_writes_required_fields_only_in_lenient_mode() {
        let mut inner = object("inner", vec![required_str("deep")]);
        inner.required = true;
        let mut tags = list("tags", node("[]", SettingKind::Str));
        tags.required = true;
        let outer = object("outer", vec![required_str("name"), inner, tags]);

        assert_eq!(
            empty_value(&outer, Policy::Strict),
            Some(json!({})),
            "严格骨架不发明假值"
        );
        assert_eq!(
            empty_value(&outer, Policy::Lenient),
            Some(json!({"name": "", "inner": {"deep": ""}, "tags": []})),
            "宽容策略只写必填字段（缺席即默认）"
        );
    }

    // ── 用法错误文案（严格策略 = CLI 的文案，逐字） ──────────

    #[test]
    fn strict_usage_error_texts_are_the_cli_ones() {
        let mut doc = json!({"gateway": {"port": 8080}, "tools": ["a", "b"], "providers": [{}]});
        assert_eq!(
            set_path(
                &mut doc,
                &steps("gateway.port.sub"),
                json!(1),
                Policy::Strict
            )
            .unwrap_err(),
            "路径 gateway.port.sub 的中间层 gateway.port 不是对象"
        );
        assert_eq!(
            set_path(
                &mut doc,
                &steps("gateway.port[0]"),
                json!(1),
                Policy::Strict
            )
            .unwrap_err(),
            "路径 gateway.port[0] 的中间层 gateway.port 不是列表"
        );
        assert_eq!(
            set_path(
                &mut doc,
                &steps("providers[9].name"),
                json!("x"),
                Policy::Strict
            )
            .unwrap_err(),
            "列表下标越界：providers 只有 1 项（下标 9）"
        );
        assert_eq!(
            set_path(&mut doc, &steps("providers[]"), json!("x"), Policy::Strict).unwrap_err(),
            "模板路径（[]）不能用于写操作"
        );
        assert_eq!(
            unset_path(&mut doc, &[], Policy::Strict).unwrap_err(),
            "路径不能为空"
        );
        assert_eq!(
            unset_path(&mut doc, &steps("providers[]"), Policy::Strict).unwrap_err(),
            "模板路径（[]）不能用于写操作"
        );
        assert_eq!(
            remove_indexed(&mut doc, &steps("tools")).unwrap_err(),
            "tools 必须以具体下标结尾（如 providers[1]）"
        );
        assert_eq!(
            remove_indexed(&mut doc, &steps("gateway.nope[0]")).unwrap_err(),
            "路径不在当前文档中：gateway.nope（先 set / add 建出来）"
        );
        assert_eq!(
            remove_indexed(&mut doc, &steps("gateway.port[0]")).unwrap_err(),
            "gateway.port 不是列表（文档里的值不是 JSON 数组）"
        );
        assert_eq!(
            remove_indexed(&mut doc, &steps("tools[5]")).unwrap_err(),
            "列表下标越界：tools 只有 2 项（下标 5）"
        );
        assert_eq!(
            move_indexed(&mut doc, &steps("tools[5]"), 1, Policy::Strict).unwrap_err(),
            "列表下标越界：tools 只有 2 项（下标 5）"
        );
        assert_eq!(
            move_indexed(&mut doc, &steps("tools[]"), 1, Policy::Strict).unwrap_err(),
            "tools[] 必须以具体下标结尾（如 providers[1]）"
        );
        assert_eq!(
            move_indexed(&mut doc, &steps("providers[].models[0]"), 1, Policy::Strict).unwrap_err(),
            "模板路径（[]）不能用于写操作"
        );
        let tools = list("tools", node("[]", SettingKind::Str));
        assert_eq!(
            append_item(
                &mut doc,
                &steps("gateway.port"),
                &tools,
                json!("x"),
                Policy::Strict
            )
            .unwrap_err(),
            "gateway.port 不是列表（文档里的值不是 JSON 数组）"
        );
    }
}
