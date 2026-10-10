# wing/config/spec.py
"""声明层：每个配置项在**字段声明处**声明一次（模型 / 默认值 / 说明 / 枚举 / 密文 / 生效域）。

为什么单独成模块：

- 元信息曾是两份人读文本（``models.py`` 的字段 docstring 与手写 YAML 模板，靠 ``# SYNC`` 注释
  维系）——机器读不到，设置界面就长不出来。声明层把它变成结构化数据，挂在
  ``FieldInfo.json_schema_extra[WING_META_KEY]`` 上（进 JSON Schema / OpenAPI，下游
  catalog（02）与设置面板（03/06）从这里读）。
- **声明取代 docstring**：迁移时字段 docstring 整段搬进 ``doc`` / ``notes``，两边都留就是新的 SYNC。
- 本模块是纯数据（无 I/O、不 import 其它 wing 包）；``S()`` 只把参数规整成一次 ``Field(...)`` 调用，
  真正的字段级约束（``default`` / ``gt`` / ``pattern`` / ...）原样透传给 pydantic。
"""

from __future__ import annotations

from enum import Enum
from typing import Any

from pydantic import BaseModel, ConfigDict, Field
from pydantic.fields import FieldInfo


class ApplyScope(str, Enum):
    """改了这个键，什么时候生效（值即 wire 字符串）。每个字段必须显式声明。"""

    #: 保存即热重载生效（provider 池重建 / 每调用现读）。
    HOT = "hot"
    #: 新建会话生效；进行中的会话保持自己的状态（模板 / agent 构造期快照 / 会话自己的 patterns）。
    NEXT_SESSION = "next_session"
    #: 进程级：必须重启网关（监听地址、启动时注册的 job 间隔）。
    RESTART = "restart"
    #: 派生值 / env 覆盖 / 只读事实，面板不可编辑。
    READONLY = "readonly"


class SettingMeta(BaseModel):
    """一个配置项的元信息（``json_schema_extra["wing"]`` 的唯一内容）。

    ``frozen``：声明是配置事实，运行期只读。
    """

    model_config = ConfigDict(frozen=True)

    doc: str
    """一行摘要（面板行内提示 + YAML 行上注释）。"""
    notes: str | None = None
    """多行详解（面板详情栏 + YAML 块注释）；02 按 ``\\n`` 切行。"""
    title: str | None = None
    """人类标签；缺省 = 字段名。"""
    apply: ApplyScope = ApplyScope.HOT
    """生效域（门禁强制每个 ``S(...)`` 调用显式写出；有默认值只是为了让模型可构造）。"""
    secret: bool = False
    """密文：只写不回显（掩码 + 末 4 位 hint）。"""
    choices: dict[str, str] | None = None
    """枚举：值 -> 含义（`Literal` 之外的补充说明；面板的内联选择项）。"""
    example: str | None = None
    """示例值（详情栏 + YAML 注释）。"""
    editable: bool = True
    """False = 面板灰显只读。"""
    deprecated: str | None = None
    """预留：废弃说明。本期不消费（无迁移 / 无 rename）。"""
    summary_fields: list[str] | None = None
    """列表项标题行的字段名序列（列表字段上声明；缺省 = 第一个标量子字段）。"""
    identity_field: str | None = None
    """列表项的**身份字段名**（列表字段上声明；元素模板里那个唯一的非密文标量字段）。

    用途唯一：保存时密文 ``null`` 哨兵的回填配对（``document.resolve_secrets`` 的
    LIST 分支）——按身份配对而不是按下标，删 / 移 / 前插列表项后密钥不会错配到别的项。
    **只在「元素子树里有可达密文叶子」的列表上声明**（没有密文叶子的列表结构变化
    不涉及密钥回填，声明是无用噪声）；没有可用身份字段的密文列表（如
    ``gateway.auth.keys``：``key`` 是密文、``role`` 不唯一）不声明，走「长度相等的
    安全下标回落 / 宁可不猜」分支。

    门禁（``tests/test_config_spec.py``）：必须指向元素模板里**真实存在**、**非密文**、
    **标量 kind** 的字段，写错即红。
    """
    min_items: int | None = None
    """「不得为空」的**声明**（强制仍由 ``problems.cross_field_problems`` 负责，见其模块 docstring）。"""
    max_items: int | None = None
    """上限声明（本期无字段使用，留接口）。"""


WING_META_KEY = "wing"
"""元信息在 ``FieldInfo.json_schema_extra`` 里的键名（读取点唯一常量，避免字面量四处重复）。"""


def setting_meta(field: FieldInfo) -> SettingMeta | None:
    """从任意 ``FieldInfo`` 取声明元信息；未声明（或形态不符）返回 ``None``。

    返回 ``None`` 就是「这个字段没有声明」——声明门禁（``tests/test_config_spec.py``）
    与 02 的 catalog builder 共用这一个读取口。
    """
    extra: Any = field.json_schema_extra
    if not isinstance(extra, dict):
        return None
    raw = extra.get(WING_META_KEY)
    if raw is None:
        return None
    return SettingMeta.model_validate(raw)


def S(
    *,
    doc: str,
    notes: str | None = None,
    title: str | None = None,
    apply: ApplyScope = ApplyScope.HOT,
    secret: bool = False,
    choices: dict[str, str] | None = None,
    example: str | None = None,
    editable: bool = True,
    deprecated: str | None = None,
    summary_fields: list[str] | None = None,
    identity_field: str | None = None,
    min_items: int | None = None,
    max_items: int | None = None,
    **field_kwargs: Any,
) -> Any:
    """声明一个配置项，返回 pydantic ``FieldInfo``（元信息挂 ``json_schema_extra["wing"]``）。

    Args:
        doc: 一行摘要（必填）。
        field_kwargs: 原样透传 ``pydantic.Field``：``default`` / ``default_factory`` / ``gt`` /
            ``pattern`` / ... **不引入第二个 ``required=``**——必填由「无 default」表达，
            catalog 读 ``FieldInfo.is_required()``（两个来源必然漂移）。
    """
    meta = SettingMeta(
        doc=doc,
        notes=notes,
        title=title,
        apply=apply,
        secret=secret,
        choices=choices,
        example=example,
        editable=editable,
        deprecated=deprecated,
        summary_fields=summary_fields,
        identity_field=identity_field,
        min_items=min_items,
        max_items=max_items,
    )
    return Field(json_schema_extra={WING_META_KEY: meta.model_dump()}, **field_kwargs)
