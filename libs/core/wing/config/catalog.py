# wing/config/catalog.py
"""设置目录树（catalog）—— 由声明递归生成的、机器可读的配置事实。

为什么单独成模块：

- 01 的声明层回答了「这个字段是什么」，本模块把它变成**一棵树**：路径文法（``providers[].api_key``）、
  类型推导（注解 → ``SettingKind``）、约束（``gt`` → ``min`` + ``exclusive_min``）、枚举值域、
  业务分组（``config/groups.py`` 的表盖到 root 直接子节点上）、列表元素形态（``element`` / ``variants``）。
  Setting API（03）、TUI 面板（07/08）、``wing config`` CLI（12）与自动生成文档共用这一份数据，
  不允许各自再推一遍类型。
- 纯逻辑：不读文件、不 import gateway / runtime / event_bus。每次 ``build_catalog()`` 现建，
  **不缓存**——配置量级是几十个节点，缓存只换来失效问题。
- ``parse_path()`` 是路径文法（总设计 §5.2）的**唯一实现**（Rust 侧 ``wing-api-client`` 有同构的一份，
  跨语言无法共享代码，两侧用同一批用例各自单测）。
"""

from __future__ import annotations

import re
import types
from dataclasses import dataclass
from enum import Enum
from typing import Annotated, Any, Literal, Union, get_args, get_origin

from annotated_types import Ge, Gt, Le, Lt
from pydantic import BaseModel, Field
from pydantic.fields import FieldInfo
from pydantic_core import PydanticUndefined

from .groups import SettingGroup, build_groups, group_of
from .models import Config
from .spec import ApplyScope, SettingMeta, setting_meta


class SettingKind(str, Enum):
    """配置项的类型（值即 wire 字符串；07 按它选编辑器）。"""

    STR = "str"
    INT = "int"
    FLOAT = "float"
    BOOL = "bool"
    ENUM = "enum"
    SECRET = "secret"
    MAP = "map"
    OBJECT = "object"
    LIST = "list"


class ChoiceSpec(BaseModel):
    """一个枚举选项：值 + 含义（含义可为空）。"""

    value: str
    doc: str | None = None


class SettingNode(BaseModel):
    """设置目录树的一个节点（**领域类型，不是 wire 模型**）。

    字段名即协议：03 的 ``gateway/protocol/settings.py`` 逐字段投影成 ``SettingNodeProto``
    （与 ``config.ModelRef`` → ``gateway/protocol.ModelDetail`` 的既有惯例一致）——
    唯一的例外是 :attr:`identity_field`（纯后端语义，不投影，见该字段的 docstring）。

    三类节点：

    - **声明节点**：来自一个字段（68 个），元信息全部来自 ``SettingMeta``；
    - **合成节点**：``root``（key/path = ``config``）、列表元素（``key = "[]"``）与 union 变体
      （key 同为 ``"[]"``，path 同元素模板）——没有字段声明，``title = key``、``doc = ""``；
    - 列表元素是 union（``list[str | ModelSpec]``）时：``element`` 为 ``None``、``variants`` 非空。
    """

    # ── 身份 ──
    key: str
    """字段名；列表元素模板 / union 变体的 key 恒为 ``"[]"``。"""
    path: str
    """规范地址（``providers[].api_key``）；列表元素模板用 ``[]``，值 / 问题路径用具体下标。"""
    title: str
    """人类标签（缺省 = ``key``，构建时落地）。"""
    doc: str
    """一行摘要（合成节点为空串）。"""
    notes: list[str] = Field(default_factory=list)
    """详解（``SettingMeta.notes`` 按行切分）。"""
    example: str | None = None
    """示例值（详情栏 + YAML 注释）。"""
    order: int = 0
    """同级声明序（= ``model_fields`` 插入序；union 变体 = 变体序）。"""

    # ── 类型与约束 ──
    kind: SettingKind
    required: bool = False
    """= ``FieldInfo.is_required()``（必填的唯一定义，不引入第二个来源）。"""
    nullable: bool = False
    """注解含 ``None``（``X | None``）。"""
    default: Any = None
    """标量默认值；object / list 恒为 ``None``（结构由 ``children`` / ``element`` 表达）。"""
    has_default: bool = False
    min: float | None = None
    """数值下界（来自 ``gt`` / ``ge``）。"""
    max: float | None = None
    """数值上界（来自 ``lt`` / ``le``）。"""
    exclusive_min: bool = False
    """``min`` 是开区间（``gt``）。"""
    exclusive_max: bool = False
    """``max`` 是开区间（``lt``）。"""
    min_length: int | None = None
    """字符串长度下限。"""
    pattern: str | None = None
    """字符串正则（前端做尽力而为的本地校验，服务端仍是权威）。"""
    choices: list[ChoiceSpec] = Field(default_factory=list)
    """枚举值域（``Literal`` args 或声明的 ``choices``）。"""
    min_items: int | None = None
    """「不得为空」的声明（= 1 时由 ``problems.cross_field_problems`` 负责强制）。"""
    max_items: int | None = None

    # ── 语义 ──
    secret: bool = False
    """密文：只写不回显（掩码 + 末 4 位 hint）。"""
    apply: ApplyScope = ApplyScope.HOT
    """生效域；容器 = 子树最粗的一档，**权威在叶子**（03 算 restart_required 只读叶子）。"""
    editable: bool = True
    deprecated: str | None = None
    """预留：废弃说明（本期不消费）。"""

    # ── 分组 ──
    section: str | None = None
    """业务分组名（只出现在 root 的直接子节点上）。

    **不是字段声明**：由 ``config/groups.py`` 的 ``SETTING_GROUPS`` 盖上来
    （:func:`build_catalog` 的最后一步）——界面分类与文件存储形式解耦，改分组只动那张表。
    """
    section_doc: str | None = None
    """分组说明（每个分组只盖在该组**声明序首个**成员上，emitter 据此写块注释）。"""

    # ── 渲染提示 ──
    summary_fields: list[str] = Field(default_factory=list)
    """列表项标题行的字段名序列（空 = 前端回落「第一个标量子字段」）。"""
    identity_field: str | None = None
    """列表项的**身份字段名**（``SettingMeta.identity_field`` 的目录投影）。

    **刻意不投影到 wire**（``SettingNodeProto`` 没有这个字段）：身份配对是纯后端语义
    （保存时密文 ``null`` 哨兵的回填配对，见 ``document.resolve_secrets``），前端不需要知道。
    """
    value_hint: str | None = None
    """值渲染提示；后端 catalog **恒为 ``None``**（只有 Rust 侧 Interface 根发 ``"color"``）。"""

    # ── 结构 ──
    children: list["SettingNode"] = Field(default_factory=list)
    """object 的子字段（声明序）。"""
    element: "SettingNode | None" = None
    """list 的单一元素类型模板。"""
    variants: list["SettingNode"] | None = None
    """list 的元素是 union 时的候选形态（声明序）。"""


ROOT_KEY = "config"
"""catalog 根节点的 key 与 path（协议增补 P2；与 Interface 根的 ``"interface"`` 对称）。"""

ROOT_TITLE = "Wing 配置"
"""根节点的人类标签。"""

ROOT_DOC = "wing-agent 后端配置（模型 / Agent / 网关 / 日志）"
"""根节点的一行摘要。"""

ELEMENT_KEY = "[]"
"""列表元素模板 / union 变体的 key（§5.1）。"""


# ─────────────────────────────────────────────────────────────────────────────
# 路径文法（§5.2）
# ─────────────────────────────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class Key:
    """一个对象字段名（``gateway`` / ``port``）。"""

    name: str


@dataclass(frozen=True, slots=True)
class Index:
    """一个具体下标（``providers[0]``）。**不做边界检查**：catalog 不知道文档有多长。"""

    index: int


@dataclass(frozen=True, slots=True)
class Element:
    """``[]``：列表元素模板。不是「第 0 个」——模板 → 具体是一对多展开（增补 P3）。"""


PathStep = Key | Index | Element
"""规范路径的一段（K·I·E 三变体，见 §5.2 的文法）。"""

_NAME_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
_INDEX_RE = re.compile(r"[0-9]+")
_INDEX_MAX = 2**64 - 1
"""下标上界（与 Rust 侧 ``usize`` 对齐；Python 整数无界，溢出要显式判）。"""


def parse_path(path: str) -> list[PathStep] | None:
    """解析规范路径；非法返回 ``None``（**不抛异常**）。

    文法::

        path    := segment ("." segment)*
        segment := name | name "[" idx "]" | name "[]"
        name    := [A-Za-z_][A-Za-z0-9_]*
        idx     := 非负整数（<= 2**64-1）

    空串非法（与 Rust 侧 ``parse_path("") == Some([])`` 有意不同——空路径在 Python 侧只可能来自
    代码 bug，判非法更安全；见 02 design.md Assumptions 3）。下标不查界（增补 P3）。
    """
    if not path:
        return None
    steps: list[PathStep] = []
    for segment in path.split("."):
        bracket = segment.find("[")
        if bracket < 0:
            name, index = segment, None
        else:
            name = segment[:bracket]
            tail = segment[bracket + 1 :]
            if not tail.endswith("]"):
                return None
            index = tail[:-1]
        if _NAME_RE.fullmatch(name) is None:
            return None
        steps.append(Key(name))
        if index is None:
            continue
        if index == "":
            steps.append(Element())
        elif _INDEX_RE.fullmatch(index) is None or int(index) > _INDEX_MAX:
            return None
        else:
            steps.append(Index(int(index)))
    return steps


# ─────────────────────────────────────────────────────────────────────────────
# 类型推导（§5.3 + 增补 P10）
# ─────────────────────────────────────────────────────────────────────────────

_APPLY_RANK: dict[ApplyScope, int] = {
    ApplyScope.HOT: 0,
    ApplyScope.NEXT_SESSION: 1,
    ApplyScope.RESTART: 2,
    ApplyScope.READONLY: 3,
}


def build_catalog(root: type[BaseModel] = Config) -> SettingNode:
    """构建设置目录树（默认根 = ``Config``；``root`` 参数只为单测用手工小模型）。

    每次调用现建、不缓存。未声明的字段直接 ``ValueError``（声明门禁已保证生产路径不出现——
    悄悄产出一个没有文档的节点比炸更糟）。

    最后一步把**业务分组**盖到 root 的直接子节点上（:func:`_stamp_groups`）：分组的
    唯一来源是 ``config/groups.py`` 的 ``SETTING_GROUPS``，字段声明里没有它。
    """
    node = SettingNode(
        key=ROOT_KEY,
        path=ROOT_KEY,
        title=ROOT_TITLE,
        doc=ROOT_DOC,
        kind=SettingKind.OBJECT,
        children=_children_of(root, ""),
    )
    node.apply = _coarsest_apply(node)
    _stamp_groups(node, root)
    return node


def _stamp_groups(node: SettingNode, root: type[BaseModel]) -> None:
    """root 的直接子节点盖上 ``section`` / ``section_doc``（分组表 → 目录节点）。

    - ``section`` = 组的 ``title``（emitter 的分隔行、``wing config list`` 的分组头、
      旧客户端的分组渲染都读它，形状不变）；
    - ``section_doc`` 只盖在**声明序首个**成员上（emitter 的一条块注释，写多处 = 重复）；
    - ``root`` 不是 :class:`Config`（单测的手工小模型）时没有分组表，一切留 ``None``；
      是 ``Config`` 而有顶层键没被任何组认领 → ``ValueError``（界面里它会无处可去）。
    """
    if root is not Config:
        return
    groups = build_groups(root)
    first_of: set[str] = set()
    for child in node.children:
        group: SettingGroup | None = group_of(child.key, groups)
        if group is None:
            raise ValueError(f"顶层配置字段没有归入任何设置分组：{child.key}")
        child.section = group.title
        if group.id not in first_of:
            first_of.add(group.id)
            child.section_doc = group.doc


def _children_of(model: type[BaseModel], prefix: str) -> list[SettingNode]:
    """一个模型的全部字段节点（``order`` = ``model_fields`` 插入序）。"""
    return [
        _field_node(name, field, order, _child_path(prefix, name))
        for order, (name, field) in enumerate(model.model_fields.items())
    ]


def _child_path(prefix: str, name: str) -> str:
    """子路径：root 的子树**不带根前缀**（``gateway.port``，与 Interface 根对称）。"""
    return f"{prefix}.{name}" if prefix else name


def _field_node(name: str, field: FieldInfo, order: int, path: str) -> SettingNode:
    """一个声明字段 → 节点（含递归 children / element / variants）。"""
    meta = setting_meta(field)
    if meta is None:
        raise ValueError(f"配置字段缺少 S(...) 声明：{path}")
    inner, nullable = _strip_optional(_unwrap(field.annotation))
    kind = _kind_of(
        inner, secret=meta.secret, has_choices=bool(meta.choices), path=path
    )

    children: list[SettingNode] = []
    element: SettingNode | None = None
    variants: list[SettingNode] | None = None
    if kind is SettingKind.OBJECT:
        children = _children_of(inner, path)
    elif kind is SettingKind.LIST:
        element, variants = _list_shape(inner, path)

    default, has_default = _default_of(field, kind)
    minimum, maximum, exclusive_min, exclusive_max = _number_bounds(field)
    min_length, pattern = _string_bounds(field)
    return SettingNode(
        key=name,
        path=path,
        title=meta.title or name,
        doc=meta.doc,
        notes=meta.notes.splitlines() if meta.notes else [],
        example=meta.example,
        order=order,
        kind=kind,
        required=field.is_required(),
        nullable=nullable,
        default=default,
        has_default=has_default,
        min=minimum,
        max=maximum,
        exclusive_min=exclusive_min,
        exclusive_max=exclusive_max,
        min_length=min_length,
        pattern=pattern,
        choices=_choices_of(inner, meta),
        min_items=meta.min_items,
        max_items=meta.max_items,
        secret=meta.secret,
        apply=meta.apply,
        editable=meta.editable,
        deprecated=meta.deprecated,
        summary_fields=list(meta.summary_fields or []),
        identity_field=meta.identity_field,
        value_hint=None,
        children=children,
        element=element,
        variants=variants,
    )


def _type_node(annotation: Any, path: str, order: int) -> SettingNode:
    """合成节点：列表元素模板 / union 变体（没有字段声明，元信息取默认值）。"""
    inner, nullable = _strip_optional(_unwrap(annotation))
    kind = _kind_of(inner, secret=False, has_choices=False, path=path)
    children: list[SettingNode] = []
    element: SettingNode | None = None
    variants: list[SettingNode] | None = None
    if kind is SettingKind.OBJECT:
        children = _children_of(inner, path)
    elif kind is SettingKind.LIST:
        element, variants = _list_shape(inner, path)
    node = SettingNode(
        key=ELEMENT_KEY,
        path=path,
        title=ELEMENT_KEY,
        doc="",
        order=order,
        kind=kind,
        nullable=nullable,
        secret=kind is SettingKind.SECRET,
        children=children,
        element=element,
        variants=variants,
    )
    node.apply = _coarsest_apply(node)
    return node


def _list_shape(
    annotation: Any, path: str
) -> tuple[SettingNode | None, list[SettingNode] | None]:
    """``list[X]`` → ``(element, None)``；``list[A | B]`` → ``(None, [variants])``。

    ``list[str | ModelSpec]`` 的 ``get_args`` 只给一个元素（那个 union 本身），所以单元素
    分支要再剥一层 union——union 的每个成员是一个候选形态（总设计 §5.1 的 ``variants``）。
    """
    args = get_args(annotation)
    element_path = f"{path}[]"
    if len(args) == 1:
        single, _ = _strip_optional(_unwrap(args[0]))
        if _is_union(single):
            return None, [
                _type_node(member, element_path, index)
                for index, member in enumerate(get_args(single))
            ]
        return _type_node(args[0], element_path, 0), None
    if len(args) > 1:
        return None, [
            _type_node(arg, element_path, index) for index, arg in enumerate(args)
        ]
    return None, None  # 裸 list：形态未知（现状无字段使用），不猜


def _kind_of(
    annotation: Any, *, secret: bool, has_choices: bool, path: str
) -> SettingKind:
    """注解 → ``SettingKind``（判定顺序写死：``bool`` 必须早于 ``int``）。

    ``has_choices`` 非空 ⇒ ``enum``（增补 P10）：值域只写在 ``notes`` 里的 ``str`` 字段
    也拿到内联选择器（``reasoning_effort`` / ``log.level``）。
    """
    if annotation is bool:
        return SettingKind.BOOL
    if annotation is int:
        return SettingKind.INT
    if annotation is float:
        return SettingKind.FLOAT
    if get_origin(annotation) is Literal:
        return SettingKind.ENUM
    if annotation is str:
        if secret:
            return SettingKind.SECRET
        return SettingKind.ENUM if has_choices else SettingKind.STR
    if annotation is dict or get_origin(annotation) is dict:
        return SettingKind.MAP
    if isinstance(annotation, type) and issubclass(annotation, BaseModel):
        return SettingKind.OBJECT
    if get_origin(annotation) is list:
        return SettingKind.LIST
    raise ValueError(f"不支持的配置字段注解：{path} → {annotation!r}")


def _choices_of(annotation: Any, meta: SettingMeta) -> list[ChoiceSpec]:
    """枚举值域：``Literal`` 的 args（按声明序）+ 声明的 ``choices`` 含义。

    - ``Literal`` 值域为准；声明的 ``choices`` 里多出来的键**忽略**（面板不允许选一个
      pydantic 会拒绝的值）；
    - 无 ``Literal`` 时（P10 的裸 ``str`` + ``choices``），值域 = 声明的 dict 键序。
    """
    declared = meta.choices or {}
    if get_origin(annotation) is Literal:
        return [
            ChoiceSpec(value=arg, doc=declared.get(arg))
            for arg in get_args(annotation)
            if isinstance(arg, str)
        ]
    return [ChoiceSpec(value=value, doc=doc) for value, doc in declared.items()]


def _default_of(field: FieldInfo, kind: SettingKind) -> tuple[Any, bool]:
    """(default, has_default)：结构字段的默认取 ``None``（结构由 children/element 表达）。"""
    if field.is_required():
        return None, False
    if field.default_factory is not None:
        if kind in (SettingKind.OBJECT, SettingKind.LIST):
            return None, True
        return field.get_default(call_default_factory=True), True
    if field.default is PydanticUndefined:
        return None, False
    return field.default, True


def _number_bounds(field: FieldInfo) -> tuple[float | None, float | None, bool, bool]:
    """``Gt`` / ``Ge`` / ``Lt`` / ``Le`` → ``(min, max, exclusive_min, exclusive_max)``。"""
    minimum: float | None = None
    maximum: float | None = None
    exclusive_min = False
    exclusive_max = False
    for item in field.metadata:
        if isinstance(item, Gt):
            minimum, exclusive_min = float(item.gt), True
        elif isinstance(item, Ge):
            minimum, exclusive_min = float(item.ge), False
        elif isinstance(item, Lt):
            maximum, exclusive_max = float(item.lt), True
        elif isinstance(item, Le):
            maximum, exclusive_max = float(item.le), False
    return minimum, maximum, exclusive_min, exclusive_max


def _string_bounds(field: FieldInfo) -> tuple[int | None, str | None]:
    """``(min_length, pattern)``：``Field(pattern=...)`` 在 pydantic 里落成通用元数据对象，
    ``min_length`` 落成 ``MinLen``——两者都按属性 duck-typing 读（不依赖私有类名）。

    ``max_length`` 本协议没有对应字段（现状无字段使用），不读。
    """
    min_length: int | None = None
    pattern: str | None = None
    for item in field.metadata:
        raw_pattern = getattr(item, "pattern", None)
        if isinstance(raw_pattern, str):
            pattern = raw_pattern
        raw_min = getattr(item, "min_length", None)
        if isinstance(raw_min, int):
            min_length = raw_min
    return min_length, pattern


def _strip_optional(annotation: Any) -> tuple[Any, bool]:
    """剥一层 ``X | None``（PEP 604 的 ``types.UnionType`` 与 ``typing.Union`` 都认）。"""
    if _is_union(annotation) and type(None) in get_args(annotation):
        rest = tuple(arg for arg in get_args(annotation) if arg is not type(None))
        if len(rest) == 1:
            return rest[0], True
    return annotation, False


def _is_union(annotation: Any) -> bool:
    origin = get_origin(annotation)
    return origin is Union or origin is types.UnionType


def _unwrap(annotation: Any) -> Any:
    """剥 ``Annotated[T, ...]``（现状无字段使用，防御性支持）。"""
    while get_origin(annotation) is Annotated:
        annotation = get_args(annotation)[0]
    return annotation


def _coarsest_apply(node: SettingNode) -> ApplyScope:
    """子树最粗的生效域（合成节点用；声明节点如实透传 ``SettingMeta.apply``）。

    「最粗」= 子树里对用户要求最高的一档（``hot < next_session < restart < readonly``）。
    权威在叶子：03 计算 ``restart_required`` 只读叶子路径的 ``apply``，结构行上的值只作提示。
    """
    worst = node.apply
    for child in node.children:
        worst = _coarser(worst, _coarsest_apply(child))
    if node.element is not None:
        worst = _coarser(worst, _coarsest_apply(node.element))
    for variant in node.variants or []:
        worst = _coarser(worst, _coarsest_apply(variant))
    return worst


def _coarser(left: ApplyScope, right: ApplyScope) -> ApplyScope:
    return left if _APPLY_RANK[left] >= _APPLY_RANK[right] else right


__all__ = [
    "ELEMENT_KEY",
    "Element",
    "Index",
    "Key",
    "PathStep",
    "ROOT_DOC",
    "ROOT_KEY",
    "ROOT_TITLE",
    "ChoiceSpec",
    "SettingKind",
    "SettingNode",
    "build_catalog",
    "parse_path",
]
