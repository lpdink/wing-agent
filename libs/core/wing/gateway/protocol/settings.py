# wing/gateway/protocol/settings.py — Setting API 协议模型

"""Setting API 的 wire 模型（4 端点：``schema`` / ``get`` / ``status`` / ``set``）。

字段名与可空性即协议（design.md §9 冻结 + protocol_addendum 的 P1 修正 / P5 / P6）——
Rust 侧镜像在 ``crates/wing-api-client/src/models.rs``，**消费方已落地**：改这里等于改协议。
几条写死的约定：

- ``summary_fields`` 是**数组不是可空**（P1 修正）：Rust 是 ``Vec<String>``，
  发 ``null`` 会让反序列化失败（``#[serde(default)]`` 只处理缺键，不处理显式 null）。
- ``base: str | None = None``（P5）：显式 ``null`` 与缺键都表示「跳过指纹检查」（CLI ``--force``）。
- ``SettingProblem.kind`` 是**字符串**（P6）：``ProblemKind`` 序列化为它的 value；未知种类
  前端必须容忍（它只用于展示与排序），所以这里不做枚举。
- ``setup_mode`` 字段本期恒为 ``False``（网关只在配置合法时才活着）；04 才让它变成真的。
"""

from __future__ import annotations

from typing import Any, Literal

from pydantic import BaseModel, Field

from wing.config import ApplyScope, SettingKind

from .system import ReloadResponse


# ============================================================
# 目录树（GET /api/settings/schema）
# ============================================================


class SettingChoice(BaseModel):
    """枚举项：取值 + 这个值是什么意思（镜像 ``config.catalog.ChoiceSpec``）。"""

    value: str = Field(description="枚举取值")
    doc: str | None = Field(default=None, description="该选项的含义；未声明时为 null")


class SettingNodeProto(BaseModel):
    """设置目录树的一个节点（领域 ``SettingNode`` 的 wire 投影）。

    ``path`` 是规范地址（§5.2 文法）：catalog 里用 ``[]`` 表示元素模板
    （``providers[].models[].id``），文档 / problems / changed 里用具体下标
    （``providers[0].models[2].id``）。根节点的 ``key`` / ``path`` 恒为 ``"config"``（P2）。
    """

    # ── 身份 ──
    key: str = Field(description='字段名；列表元素模板 / union 变体的 key 恒为 "[]"')
    path: str = Field(description="规范地址（列表元素模板用 []）")
    title: str = Field(description="人类标签（缺省 = key）")
    doc: str = Field(description="一行摘要")
    notes: list[str] = Field(default_factory=list, description="详解（按行切分）")
    example: str | None = Field(default=None, description="示例值")
    order: int = Field(default=0, description="同级声明序")

    # ── 类型与约束 ──
    kind: SettingKind = Field(description="取值类型（值即 wire 字符串）")
    required: bool = Field(default=False, description="必填（= 字段无默认值）")
    nullable: bool = Field(default=False, description="注解含 None")
    default: Any = Field(
        default=None,
        description="标量默认值；object / list 恒为 null（结构由 children / element 表达）",
    )
    has_default: bool = Field(default=False, description="是否声明过默认值")
    min: float | None = Field(default=None, description="数值下界（来自 gt / ge）")
    max: float | None = Field(default=None, description="数值上界（来自 lt / le）")
    exclusive_min: bool = Field(default=False, description="下界是开区间（gt）")
    exclusive_max: bool = Field(default=False, description="上界是开区间（lt）")
    min_length: int | None = Field(default=None, description="字符串长度下限")
    pattern: str | None = Field(default=None, description="字符串正则")
    choices: list[SettingChoice] = Field(
        default_factory=list, description="枚举值域（kind == enum）"
    )
    min_items: int | None = Field(
        default=None, description='列表最小长度（"不得为空" = 1）'
    )
    max_items: int | None = Field(default=None, description="列表上限声明")

    # ── 语义 ──
    secret: bool = Field(default=False, description="密文：只写不回显")
    apply: ApplyScope = Field(
        default=ApplyScope.HOT,
        description="生效域；容器 = 子树最粗一档，权威在叶子",
    )
    editable: bool = Field(default=True, description="面板可编辑（false = 灰显只读）")
    deprecated: str | None = Field(
        default=None, description="预留：废弃说明（本期不消费）"
    )

    # ── 分组 ──
    section: str | None = Field(default=None, description="顶层分组名")
    section_doc: str | None = Field(
        default=None, description="分组说明（每节只写一次）"
    )

    # ── 结构 ──
    children: list["SettingNodeProto"] = Field(
        default_factory=list, description="object 的子字段"
    )
    element: "SettingNodeProto | None" = Field(
        default=None, description="list 的单一元素类型模板"
    )
    variants: list["SettingNodeProto"] | None = Field(
        default=None, description="list 的元素是 union 时的候选形态（如 models）"
    )

    # ── 渲染提示 ──
    summary_fields: list[str] = Field(
        default_factory=list,
        description='列表项标题行的字段名序列；空 = 前端回落"第一个标量子字段"',
    )
    value_hint: str | None = Field(
        default=None,
        description='值渲染提示；后端 catalog 恒发 null（"color" 只由 Rust 侧 Interface 根声明）',
    )


SettingNodeProto.model_rebuild()


class SettingsSchemaResponse(BaseModel):
    """GET /api/settings/schema 响应——设置目录（catalog 树）+ 版本 + 文件位置。"""

    version: str = Field(description="网关版本（前端可据此提示「网关比面板新」）")
    root: SettingNodeProto = Field(description="Config 的节点（catalog 的根）")
    config_path: str = Field(description="config.yaml 的绝对路径（面板标题栏展示）")


# ============================================================
# 问题与密文状态（GET /api/settings/get）
# ============================================================


class SettingProblem(BaseModel):
    """一条配置问题（领域 ``ConfigProblem`` 的 wire 投影）。

    ``kind`` 是 ``ProblemKind`` 的 value 字符串（P6）；未知种类必须容忍——种类只用于
    展示与排序（严重度见 design.md §14.1），前端不得因未知值而失败。
    """

    path: str | None = Field(
        default=None,
        description="规范路径（含具体下标）；null = 文档级（如 YAML 语法错）",
    )
    kind: str = Field(
        description="问题分类：missing_required / invalid_value / empty_list / duplicate / "
        "unknown_reference / conflict / unknown_key"
    )
    message: str = Field(description="人类可读描述")
    hint: str | None = Field(default=None, description="可操作建议")


class SecretState(BaseModel):
    """单个密文字段的状态（只写不回显：真实值不出网关）。"""

    state: Literal["set", "empty", "absent"] = Field(
        description="set = 非空 / empty = 存在但是空串 / absent = 键不在文档里"
    )
    hint: str | None = Field(
        default=None,
        description="值长度 ≥ 8 时的末 4 位；否则 null（短密钥不给 hint，避免泄露比例过高）",
    )


class SettingsGetResponse(BaseModel):
    """GET /api/settings/get 响应——稀疏文档 + 指纹 + 密文状态 + 问题。

    ``values`` 里的密文叶子恒为 ``null``；**前端必须原样回传 null**（``null`` = 保留磁盘现值，
    丢键 = 清空密钥 = 用户下次调用 401）。
    """

    values: dict[str, Any] = Field(
        default_factory=dict,
        description="稀疏文档（只有用户显式写下的键；密文叶子 = null）",
    )
    secrets: dict[str, SecretState] = Field(
        default_factory=dict, description="密文状态表，键 = 规范路径"
    )
    fingerprint: str = Field(
        description='磁盘文件的 sha256 指纹（乐观并发的唯一凭据；文件不存在 = "absent"）'
    )
    problems: list[SettingProblem] = Field(
        default_factory=list, description="当前全部问题"
    )
    setup_mode: bool = Field(default=False, description="网关此刻是否处于 setup mode")
    config_path: str = Field(description="config.yaml 的绝对路径")


class SettingsStatusResponse(BaseModel):
    """GET /api/settings/status 响应——启动路径上的最便宜预检。"""

    valid: bool = Field(description="当前配置是否可加载（false ⇒ 网关处于 setup mode）")
    setup_mode: bool = Field(default=False, description="网关此刻是否处于 setup mode")
    problems: list[SettingProblem] = Field(
        default_factory=list, description="当前全部问题"
    )
    fingerprint: str | None = Field(default=None, description="磁盘文件的 sha256 指纹")


# ============================================================
# 保存（POST /api/settings/set）
# ============================================================


class SettingsSetRequest(BaseModel):
    """POST /api/settings/set 请求——全文档替换 + 乐观并发指纹。

    密文三态（§7.5）：``null`` = 保留现值 / 字符串 = 设为该值（``""`` = 显式清空）/
    键缺席 = 该项不被覆盖（从文件移除）。
    """

    base: str | None = Field(
        default=None,
        description="保存基线指纹；null = 不做并发检查（CLI --force 的路径）",
    )
    document: dict[str, Any] = Field(description="稀疏文档（密文 null = 保留）")


class SettingsSetResponse(BaseModel):
    """POST /api/settings/set 响应——保存回执。

    **校验失败不是 HTTP 4xx**：请求本身合法，是用户填的内容不合法，所以走
    ``HTTP 200 + ok=false + problems``（总设计 D16，用户已批准）。HTTP 错误码只留给
    协议级失败（409 指纹不匹配 / 401·403 鉴权 / 500 写盘失败）。
    """

    ok: bool = Field(description="事务是否成功（false 时文件一个字节都没写）")
    fingerprint: str = Field(description="落盘后的新指纹（ok=false 时是磁盘当前指纹）")
    problems: list[SettingProblem] = Field(
        default_factory=list, description="校验问题（ok=false 时非空）"
    )
    changed: list[str] = Field(
        default_factory=list, description="相对保存前的变更路径（规范路径，含具体下标）"
    )
    restart_required: list[str] = Field(
        default_factory=list, description="变更里生效域为 restart 的路径"
    )
    reload: ReloadResponse | None = Field(
        default=None, description="热重载逐项结果（复用 /api/system/reload 的形状）"
    )
    setup_mode_exited: bool = Field(
        default=False, description="这次保存是否让网关从 setup mode 转入正常模式（04）"
    )
    backup_path: str | None = Field(
        default=None, description="备份文件路径；没有旧文件时为 null"
    )
    warnings: list[str] = Field(
        default_factory=list,
        description="非致命告知：如「原配置文件无法解析，其中的密钥无法保留，请重新填写」"
        "（04/AD13），或「密钥已被丢弃 / 按位置保留」（审查 A1 / B1）。"
        "与 problems 的区别：problems 让保存失败，warnings 只是提醒；"
        "旧前端忽略该字段即可（Rust 镜像由集成时统一补）",
    )


__all__ = [
    "SecretState",
    "SettingChoice",
    "SettingNodeProto",
    "SettingProblem",
    "SettingsGetResponse",
    "SettingsSchemaResponse",
    "SettingsSetRequest",
    "SettingsSetResponse",
    "SettingsStatusResponse",
]
