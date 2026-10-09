# wing/config/document.py
"""``config.yaml`` 的稀疏文档视图 —— 读 / 合并默认值 / 密文三态 / 差异 / 问题定位。

为什么单独成模块：

- 01 的声明层与 02 的 catalog 回答「配置长什么样」，本模块回答「**盘上那份文件长什么样**」：
  稀疏文档（只有用户显式写下的键）、sha256 指纹（乐观并发的唯一凭据）、schema 之外的未知键
  （前向兼容，原样写回）、密文三态（只写不回显）、pydantic 校验错误 → 带精确路径的问题列表。
- **服务端不缓存文档**（总设计 §20 D18）：每次 `read_document()` 现读磁盘，永远与文件一致，
  没有失效问题（config.yaml 只有几 KB）。
- 本模块是**唯一新增的文件 I/O 点**（读 `config.yaml`；写盘在 `emit` + `common/fs` 的原子写，
  由 `runtime.apply_settings` 编排）。纯逻辑 + 一次读文件，不 import gateway / runtime / event_bus。
- 路径文法复用 02 的 ``parse_path`` / ``PathStep``（``Key`` / ``Index`` / ``Element``）——
  不写第二个地址解析器；``node_at_path`` 是 Rust 侧 ``SettingNode::node_at`` 的 Python 对偶。

三个消费方（都在 L4）：``GET /api/settings/get``（掩码 + 状态 + problems）、
``GET /api/settings/status``（valid 判定）、``runtime.apply_settings``（保存事务的前四步）。
"""

from __future__ import annotations

import hashlib
from collections.abc import Iterable, Iterator, Mapping
from dataclasses import dataclass, field
from typing import Any, Literal, get_args

import yaml
from pydantic import BaseModel, ValidationError

from .catalog import (
    Key,
    SettingKind,
    SettingNode,
    build_catalog,
    parse_path,
)
from .emit import _item_template
from .loader import get_config_path
from .models import Config
from .problems import ConfigProblem, ProblemKind, model_name_message
from .spec import ApplyScope

ABSENT_FINGERPRINT = "absent"
"""文件不存在时的指纹值（乐观并发仍可判定：缺席 ⇔ 缺席）。"""

UNPARSEABLE_CONFIG_HINT = (
    "配置文件无法解析；请手工检查语法或备份后删除该文件让 wing 重新生成"
)
"""「连字段级校验都做不出来」的问题的建议（AD12 的兜底文案，boot 与设置端点共用）。"""

_MISSING = object()
"""「键缺席」（与显式 ``None`` 区分：``null`` 是用户写下的值）。"""


# ─────────────────────────────────────────────────────────────────────────────
# 数据模型
# ─────────────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class SparseDocument:
    """``config.yaml`` 的稀疏视图：只有用户显式写下的键。

    ``data`` 只含声明内的键（未知键在读取时就分离进 ``extra``）；``extra`` 是其
    父容器路径 + 键名（``gateway.weird`` / ``providers[0].foo``），由 emitter 原样写回。
    """

    data: dict[str, Any]
    extra: list[tuple[str, Any]] = field(default_factory=list)


@dataclass(frozen=True)
class ConfigFingerprint:
    """文件指纹：乐观并发的唯一凭据（``sha256(file bytes)`` 的 hex；文件不存在 = ``absent``）。"""

    value: str


@dataclass(frozen=True)
class SecretState:
    """单个密文字段在磁盘上的状态（只写不回显：真实值不出网关，只给末 4 位提示）。"""

    state: Literal["set", "empty", "absent"]
    hint: str | None = None
    """值长度 ≥ 8 时的末 4 位；否则 ``None``（短密钥不给 hint，避免泄露比例过高）。"""


class ConfigDocumentError(ValueError):
    """文件存在但读不出稀疏文档（YAML 语法错 / 顶层不是映射）。

    携带文件**指纹**：文件字节仍可作乐观并发的凭据——解析失败不该让 409 判定失效
    （内容变了 vs 内容坏了，是两件事）。
    """

    def __init__(self, message: str, fingerprint: str) -> None:
        super().__init__(message)
        self.fingerprint = fingerprint

    def as_problem(self) -> ConfigProblem:
        """坏文件 → 一条文档级 problem（``path=None``）——三个消费方共用同一份文案。"""
        return ConfigProblem(
            path=None,
            kind=ProblemKind.INVALID_VALUE,
            message=str(self),
            hint="修复 config.yaml（或从 config.yaml.bak 恢复）后重试",
        )


# ─────────────────────────────────────────────────────────────────────────────
# 读取
# ─────────────────────────────────────────────────────────────────────────────


def read_document() -> tuple[SparseDocument, ConfigFingerprint]:
    """现读磁盘 → ``(稀疏文档, 指纹)``。

    Raises:
        ConfigDocumentError: 文件存在但不是合法 YAML / 顶层不是映射（带指纹）。
            文件**缺席**不是错误：``data={}`` + 指纹 ``absent``。
    """
    path = get_config_path()
    try:
        raw = path.read_bytes()
    except FileNotFoundError:
        return SparseDocument(data={}), ConfigFingerprint(value=ABSENT_FINGERPRINT)

    fingerprint = ConfigFingerprint(value=hashlib.sha256(raw).hexdigest())
    try:
        parsed = yaml.safe_load(raw.decode("utf-8"))
    except (yaml.YAMLError, UnicodeDecodeError) as exc:
        raise ConfigDocumentError(
            f"config.yaml 不是合法 YAML：{exc}", fingerprint.value
        ) from exc
    if parsed is None:
        parsed = {}
    if not isinstance(parsed, dict):
        raise ConfigDocumentError(
            f"config.yaml 顶层必须是映射，实得 {type(parsed).__name__}",
            fingerprint.value,
        )

    extra: list[tuple[str, Any]] = []
    data = _split_known(build_catalog(), parsed, "", extra)
    return SparseDocument(data=data, extra=extra), fingerprint


def _split_known(
    node: SettingNode, value: Any, path: str, extra: list[tuple[str, Any]]
) -> Any:
    """递归把 schema 之外的键收进 ``extra``（freeform map 的内部键不算未知键）。

    未知键**不丢**（总设计 D17）：路径带上父容器（``gateway.weird``），emitter 据此
    在父节末尾原样写回。
    """
    if node.kind is SettingKind.OBJECT and isinstance(value, dict):
        known = {child.key: child for child in node.children}
        out: dict[Any, Any] = {}
        for key, item in value.items():
            child_path = _join(path, key)
            child = known.get(key)
            if child is None:
                extra.append((child_path, item))
            else:
                out[key] = _split_known(child, item, child_path, extra)
        return out
    if node.kind is SettingKind.LIST and isinstance(value, list):
        out_list: list[Any] = []
        for index, item in enumerate(value):
            template = _item_template(node, item)
            item_path = f"{path}[{index}]"
            if template is None:
                out_list.append(item)
            else:
                out_list.append(_split_known(template, item, item_path, extra))
        return out_list
    return value


def _join(path: str, key: Any) -> str:
    """子路径：root 的子树不带根前缀（与 catalog 的路径生成同一条规则）。"""
    return f"{path}.{key}" if path else str(key)


# ─────────────────────────────────────────────────────────────────────────────
# 合并默认值
# ─────────────────────────────────────────────────────────────────────────────


def merge_with_defaults(doc: SparseDocument, catalog: SettingNode) -> dict[str, Any]:
    """稀疏文档 + 声明默认值 → 完整文档（喂 ``Config(**raw)`` 校验 / 宽容视图）。

    pydantic 自己也会为缺席键补默认；合并的实质收益在**宽容视图**
    （``Config.model_construct(**raw)`` 不做默认填充）——跨字段检查因此看到与
    ``Config(**raw)`` 一致的有效配置。**不会**把「必填但缺席」变成有值：那正是要报的错。
    """
    merged = _merge(catalog, doc.data)
    return merged if isinstance(merged, dict) else {}


def _merge(node: SettingNode, value: Any) -> Any:
    if node.kind is SettingKind.OBJECT and isinstance(value, dict):
        out = dict(value)
        for child in node.children:
            if child.key in out:
                out[child.key] = _merge(child, out[child.key])
            elif child.has_default:
                out[child.key] = _materialize_default(child)
        return out
    if node.kind is SettingKind.LIST and isinstance(value, list):
        out_list: list[Any] = []
        for item in value:
            template = _item_template(node, item)
            out_list.append(_merge(template, item) if template is not None else item)
        return out_list
    return value


def _materialize_default(node: SettingNode) -> Any:
    """声明默认值 → 具体值（缺席的 object 递归展开；list / map 取空容器）。"""
    if node.kind is SettingKind.OBJECT:
        out: dict[str, Any] = {}
        for child in node.children:
            if child.has_default:
                out[child.key] = _materialize_default(child)
        return out
    if node.kind is SettingKind.LIST:
        return node.default if isinstance(node.default, list) else []
    if node.kind is SettingKind.MAP:
        return node.default if isinstance(node.default, dict) else {}
    return node.default


# ─────────────────────────────────────────────────────────────────────────────
# 密文（只写不回显）
# ─────────────────────────────────────────────────────────────────────────────


def resolve_secrets(
    incoming: SparseDocument, current: SparseDocument, catalog: SettingNode
) -> SparseDocument:
    """密文三态回填（总设计 §7.5）：``null`` → 取 current 的值；字符串 → 保留；缺席 → 不覆盖。

    - ``null`` 是「保留磁盘现值」的哨兵（前端从 ``get`` 拿到的就是它，必须原样回传）；
      current 也没有该值时，键从文档里**移除**（必填字段随之成为 problem，绝不静默清空）。
    - 键缺席 = 该项不被覆盖（从文件移除）。
    - 返回**新**文档（不改入参：``data`` 的容器逐层重建，未触碰的子树按引用共享——只读）。
    """
    data = _resolve(catalog, incoming.data, current.data)
    return SparseDocument(
        data=data if isinstance(data, dict) else {}, extra=incoming.extra
    )


def _resolve(node: SettingNode, value: Any, current: Any) -> Any:
    if node.secret and node.kind is SettingKind.SECRET:
        if value is None:
            return _MISSING if current is _MISSING else current
        return value
    if node.kind is SettingKind.OBJECT and isinstance(value, dict):
        known = {child.key: child for child in node.children}
        out: dict[Any, Any] = {}
        for key, item in value.items():
            child = known.get(key)
            if child is None:
                out[key] = item
                continue
            current_item = (
                current.get(key, _MISSING) if isinstance(current, dict) else _MISSING
            )
            resolved = _resolve(child, item, current_item)
            if resolved is not _MISSING:
                out[key] = resolved
        return out
    if node.kind is SettingKind.LIST and isinstance(value, list):
        current_list = current if isinstance(current, list) else []
        out_list: list[Any] = []
        for index, item in enumerate(value):
            template = _item_template(node, item)
            if template is None:
                out_list.append(item)
                continue
            current_item = (
                current_list[index] if index < len(current_list) else _MISSING
            )
            resolved = _resolve(template, item, current_item)
            if resolved is not _MISSING:
                out_list.append(resolved)
        return out_list
    return value


def secret_states(doc: SparseDocument, catalog: SettingNode) -> dict[str, SecretState]:
    """文档里可达的密文叶子 → 状态表（``providers[i].api_key`` / ``gateway.auth.keys[i].key``）。

    列表按文档实际存在的下标枚举；对象存在但键缺席 → ``absent``（面板据此渲染
    ``(not set)``）。对象本身缺席时**不**产出条目（没有下标可枚举）。
    """
    return {
        path: _state_of(value) for path, value in _secret_leaves(catalog, doc.data, "")
    }


def mask_secrets(doc: SparseDocument, catalog: SettingNode) -> dict[str, Any]:
    """稀疏文档的掩码副本：**每个存在的** secret 叶子替换为 ``null``（set 与 empty 都掩码）。

    前端不靠值的形状区分三态——那正是 ``secrets`` 表的职责；`values` 里留下真值或空串
    只会给「回传时会不会把密钥写丢」增加歧义。
    """
    masked = _mask(catalog, doc.data)
    return masked if isinstance(masked, dict) else {}


def _secret_leaves(
    node: SettingNode, value: Any, path: str
) -> Iterator[tuple[str, Any]]:
    """产出可达的 secret 叶子 ``(规范路径, 值)``；值缺席时给 ``_MISSING``。"""
    if node.secret and node.kind is SettingKind.SECRET:
        yield path, value
        return
    if node.kind is SettingKind.OBJECT and isinstance(value, dict):
        for child in node.children:
            yield from _secret_leaves(
                child, value.get(child.key, _MISSING), _join(path, child.key)
            )
    elif node.kind is SettingKind.LIST and isinstance(value, list):
        for index, item in enumerate(value):
            template = _item_template(node, item)
            if template is not None:
                yield from _secret_leaves(template, item, f"{path}[{index}]")


def _mask(node: SettingNode, value: Any) -> Any:
    if node.secret and node.kind is SettingKind.SECRET:
        return None if value is not _MISSING else _MISSING
    if node.kind is SettingKind.OBJECT and isinstance(value, dict):
        known = {child.key: child for child in node.children}
        out: dict[Any, Any] = {}
        for key, item in value.items():
            child = known.get(key)
            out[key] = _mask(child, item) if child is not None else item
        return out
    if node.kind is SettingKind.LIST and isinstance(value, list):
        out_list: list[Any] = []
        for item in value:
            template = _item_template(node, item)
            out_list.append(_mask(template, item) if template is not None else item)
        return out_list
    return value


def _state_of(value: Any) -> SecretState:
    """值 → 三态 + hint。无法判定（手写坏文件里的非字符串）不猜：``None`` = empty，其余 = set。"""
    if value is _MISSING:
        return SecretState(state="absent")
    if isinstance(value, str):
        if value == "":
            return SecretState(state="empty")
        hint = value[-4:] if len(value) >= 8 else None
        return SecretState(state="set", hint=hint)
    return SecretState(state="empty" if value is None else "set")


# ─────────────────────────────────────────────────────────────────────────────
# 问题定位（pydantic ValidationError → 带精确路径的 ConfigProblem）
# ─────────────────────────────────────────────────────────────────────────────


def locate_problems(raw: dict[str, Any]) -> list[ConfigProblem]:
    """``Config(**raw)`` → ``ValidationError`` → 带规范路径的问题列表（**不抛异常**）。

    只报**字段级**错误：模型级校验器的错误（``loc == ()``）由 ``cross_field_problems``
    以精确路径给出同一文案的结构化版本（见 :func:`_translate_errors`）。

    形态错误（顶层键不是字符串 / 值不是映射等，pydantic 抛 ``TypeError``）同样不抛：
    报一条 ``path=None`` 的文档级问题。这是**公开面**——启动读取（``boot_config``）与
    设置端点的三个读路径都直接调它，任何输入形态都必须能产出「问题清单」而不是 500 / 崩溃
    （AD12）。
    """
    try:
        Config(**raw)
    except ValidationError as exc:
        return _translate_errors(exc.errors(), raw)
    except Exception as exc:  # noqa: BLE001 — 兜底是本函数的契约（文档级问题，不抛）
        return [malformed_document_problem(exc)]
    return []


def malformed_document_problem(exc: BaseException) -> ConfigProblem:
    """「连一次字段级校验都做不出来」的文档 → 一条 ``path=None`` 的问题。

    文案与 ``boot_config()`` 的最外层兜底同源（同一个出口，两处消费方读同一句话）。
    """
    return ConfigProblem(
        path=None,
        kind=ProblemKind.INVALID_VALUE,
        message=f"config.yaml 无法解析：{exc}",
        hint=UNPARSEABLE_CONFIG_HINT,
    )


def _translate_errors(
    errors: Iterable[Mapping[str, Any]], raw: dict[str, Any]
) -> list[ConfigProblem]:
    translated: list[tuple[_LocInfo, ConfigProblem]] = []
    for err in errors:
        loc = tuple(err.get("loc", ()))
        if err.get("type") == "value_error" and not loc:
            # 模型级校验器（``Config._validate_config`` 的薄适配器）的忠实回声：
            # 跨字段检查已经报过同一条文案（带精确路径），保留它只会在面板里多出
            # 一行「不可导航的重复项」。
            continue
        message = _clean_message(err)
        kind = (
            ProblemKind.MISSING_REQUIRED
            if err.get("type") == "missing"
            else ProblemKind.INVALID_VALUE
        )
        info = _loc_info(loc)
        path = _rehome_bare_model_name(loc, message, raw) or info.path
        translated.append(
            (info, ConfigProblem(path=path or None, kind=kind, message=message))
        )

    # union 分支试错（loc 末段是成员标签）只在**该分支没有更精确的错误**时才保留：
    # 否则每个对象形态的字段错误都会附赠一条「Input should be a valid string」
    # （str 分支试错）。反过来，两条都是分支错误时（如 ``models: [123]``）必须保留
    # 至少一条——「problems 为空 ⇒ Config(**raw) 通过」是保存事务的硬不变量。
    precise_groups = {
        info.group
        for info, _ in translated
        if info.group is not None and not info.is_branch
    }
    return [
        problem
        for info, problem in translated
        if not (info.is_branch and info.group in precise_groups)
    ]


def _clean_message(err: Mapping[str, Any]) -> str:
    """pydantic 的 ``msg`` 剥掉 ``"Value error, "`` 包装前缀——文案本体是校验器自己写的。"""
    return str(err.get("msg", "")).removeprefix("Value error, ")


@dataclass(frozen=True)
class _LocInfo:
    """一条 ``loc`` 的翻译结果。"""

    path: str
    """规范路径（union 成员标签不进路径）。"""
    group: tuple[Any, ...] | None
    """union 分支组（首个成员标签之前的前缀）；没有 union 时为 ``None``。"""
    is_branch: bool
    """这条错误是 union 的**分支试错**（``loc`` 末段是成员标签）。"""


def _loc_info(loc: tuple[Any, ...]) -> _LocInfo:
    """pydantic 的 ``loc`` → 规范路径 + union 分支信息。

    union 成员标签的识别**不硬编码类型名**：从 ``Config.model_fields`` 出发维护
    「当前位置的候选模型集合」——是候选模型的字段就追加并下钻，否则按类名过滤候选集合
    后跳过（pydantic 对 ``list[str | ModelSpec]`` 的每个成员各报一条错误，
    标签是成员不是字段）。实测标签形如 ``('providers', 0, 'models', 0, 'str')`` /
    ``(..., 'ModelSpec', 'name')``。
    """
    parts: list[str] = []
    candidates: tuple[type[BaseModel], ...] = (Config,)
    group: tuple[Any, ...] | None = None
    tag_at: int | None = None
    for position, item in enumerate(loc):
        if isinstance(item, int):
            if parts:  # 下标属于前一段（`a[0]`），孤立的 int 段理论上不存在
                parts[-1] = f"{parts[-1]}[{item}]"
            continue
        model = next((m for m in candidates if item in m.model_fields), None)
        if model is None:
            if group is None:
                group = tuple(loc[:position])
            tag_at = position
            candidates = tuple(m for m in candidates if m.__name__ == item)
            continue
        parts.append(item)
        candidates = _models_below(model.model_fields[item].annotation)
    return _LocInfo(
        path=".".join(parts),
        group=group,
        is_branch=tag_at is not None and tag_at == len(loc) - 1,
    )


def _models_below(annotation: Any) -> tuple[type[BaseModel], ...]:
    """注解 → 该位置可能出现的候选模型（递归收集 union / list / Optional 里的 BaseModel）。"""
    found: list[type[BaseModel]] = []
    _collect_models(annotation, found)
    return tuple(found)


def _collect_models(annotation: Any, out: list[type[BaseModel]]) -> None:
    """递归收集注解里的 ``BaseModel`` 子类（``Annotated`` 的 metadata 不是类型，自动跳过）。"""
    if isinstance(annotation, type):
        if issubclass(annotation, BaseModel):
            out.append(annotation)
        return
    for arg in get_args(annotation):
        _collect_models(arg, out)


def _rehome_bare_model_name(
    loc: tuple[Any, ...], message: str, raw: dict[str, Any]
) -> str | None:
    """增补 P9：裸字符串形态的调用名非法时，pydantic 把 ``loc`` 记成 ``providers[i].name``。

    归到 **``providers[i].models``（列表级）**——pydantic 没给出下标，猜下标会指错行。
    用文案精确区分「provider 名非法」与「模型名非法」：两者共享同一个 loc，
    但只有后者的文案等于 ``model_name_message(裸字符串)``。
    """
    if (
        len(loc) < 3
        or loc[-1] != "name"
        or not isinstance(loc[-2], int)
        or loc[-3] != "providers"
    ):
        return None
    index = loc[-2]
    providers = raw.get("providers")
    if not isinstance(providers, list) or index >= len(providers):
        return None
    provider = providers[index]
    models = provider.get("models") if isinstance(provider, dict) else None
    if not isinstance(models, list):
        return None
    if not any(
        isinstance(item, str) and model_name_message(item) == message for item in models
    ):
        return None
    return f"providers[{index}].models"


# ─────────────────────────────────────────────────────────────────────────────
# 差异（保存回执的 changed / restart_required）
# ─────────────────────────────────────────────────────────────────────────────


def changed_paths(
    old: SparseDocument, new: SparseDocument, catalog: SettingNode
) -> list[str]:
    """两棵稀疏文档的递归 diff → 变更路径（added / removed / modified 合并成一个列表）。

    - 对象按字段递归、列表按**下标**递归（元素模板按文档实际形状选）——路径精确到叶子；
    - 缺席与空容器等价（``{}`` ⇔ 缺席、``[]`` ⇔ 缺席）：语义上没有变化就不报；
    - 一侧是标量而另一侧是容器 → 记该节点路径后停（不猜子路径）；
    - 比较对类型敏感（``True != 1``、``300 != 300.0``）——盘上的字节确实变了。
    """
    out: list[str] = []
    _diff(catalog, old.data, new.data, "", out)
    return out


def _diff(
    node: SettingNode, old_value: Any, new_value: Any, path: str, out: list[str]
) -> None:
    if node.kind is SettingKind.OBJECT:
        if not _is_container(old_value, dict) or not _is_container(new_value, dict):
            if not _same(old_value, new_value):
                out.append(path)
            return
        old_map = old_value if isinstance(old_value, dict) else {}
        new_map = new_value if isinstance(new_value, dict) else {}
        for child in node.children:
            _diff(
                child,
                old_map.get(child.key, _MISSING),
                new_map.get(child.key, _MISSING),
                _join(path, child.key),
                out,
            )
        return
    if node.kind is SettingKind.LIST:
        if not _is_container(old_value, list) or not _is_container(new_value, list):
            if not _same(old_value, new_value):
                out.append(path)
            return
        old_list = old_value if isinstance(old_value, list) else []
        new_list = new_value if isinstance(new_value, list) else []
        for index in range(max(len(old_list), len(new_list))):
            old_item = old_list[index] if index < len(old_list) else _MISSING
            new_item = new_list[index] if index < len(new_list) else _MISSING
            template = _item_template(
                node, new_item if new_item is not _MISSING else old_item
            )
            item_path = f"{path}[{index}]"
            if template is None:
                if not _same(old_item, new_item):
                    out.append(item_path)
            else:
                _diff(template, old_item, new_item, item_path, out)
        return
    if not _same(old_value, new_value):
        out.append(path)


def _is_container(value: Any, kind: type) -> bool:
    """缺席视为空容器（``{}`` ⇔ 缺席）；标量则不是。"""
    return value is _MISSING or isinstance(value, kind)


def _same(left: Any, right: Any) -> bool:
    """值相等（**含类型**）：``True != 1``、``300 != 300.0``。"""
    if left is _MISSING or right is _MISSING:
        return left is right
    return type(left) is type(right) and bool(left == right)


def node_at_path(catalog: SettingNode, path: str) -> SettingNode | None:
    """按规范路径查 catalog 节点（Rust ``SettingNode::node_at`` 的 Python 对偶）。

    - 根节点的 ``key`` 是**可选**前缀（``config.gateway.port`` 与 ``gateway.port`` 等价）；
    - 下标**不查界**（catalog 是模板，不知道文档有多长）；
    - 列表声明的是 ``variants``（union 元素）时返回 ``None``——元素形态只有文档里的
      实际值才能判定（调用方改用 ``_item_template`` 口径）；路径非法同样 ``None``。
    """
    steps = parse_path(path)
    if steps is None:
        return None
    node = catalog
    for step in steps:
        if isinstance(step, Key):
            child = next((c for c in node.children if c.key == step.name), None)
            if child is None:
                if node is catalog and step.name == catalog.key:
                    continue
                return None
            node = child
        else:  # Index / Element：都走元素模板
            if node.element is None:
                return None
            node = node.element
    return node


def restart_required_paths(changed: list[str], catalog: SettingNode) -> list[str]:
    """变更里 apply == ``restart`` 的路径（增补 P13：**只读叶子**的节点）。

    容器节点的 apply 是子树最粗的一档（01 的约定），用它判定会误报/漏报；
    解析不出的路径（``variants`` 上的下标等）直接跳过——宁可漏报也不误报。
    """
    out: list[str] = []
    for path in changed:
        node = node_at_path(catalog, path)
        if node is None or node.kind in (SettingKind.OBJECT, SettingKind.LIST):
            continue
        if node.apply is ApplyScope.RESTART:
            out.append(path)
    return out


__all__ = [
    "ABSENT_FINGERPRINT",
    "ConfigDocumentError",
    "ConfigFingerprint",
    "SecretState",
    "SparseDocument",
    "changed_paths",
    "locate_problems",
    "mask_secrets",
    "merge_with_defaults",
    "node_at_path",
    "read_document",
    "resolve_secrets",
    "restart_required_paths",
    "secret_states",
]
