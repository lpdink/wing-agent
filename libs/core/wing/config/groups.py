# wing/config/groups.py
"""设置目录的**业务分组**（界面分类的唯一声明来源）。

为什么单独成模块（而不是继续写在字段声明里）：

- 「配置在文件里的存储形式」（``config.yaml`` 的顶层键）与「界面里的分类」是两件事。
  分组曾以 ``S(section=...)`` 的形式散落在 13 个顶层字段上（``section_doc`` 只许写在每段
  首字段——一条靠纪律维系的约定），于是**改一次界面分类要动模型声明**，而模型声明同时是
  文件布局与校验的事实来源：两者被不必要地耦合在一起。
- 这里把分组收敛成一张**有序表**：名字 / 顺序 / 说明 / 成员一处声明。加组、并组、改名、
  调顺序都只动这张表；``config.yaml`` 的顶层键一个都不动（零迁移）。
- 消费方：``catalog.build_catalog()``（把分组盖到 root 的直接子节点上，emitter 的
  ``# ── Name ───`` 分隔行与 ``wing config list`` 的分组头都由此而来）·
  ``catalog.build_groups()``（``GET /api/settings/schema`` 的 ``groups``，前端导航的
  **唯一锚定来源**：左列锚点不许硬编码组名 / 顺序 / 成员）。

纯数据 + 纯函数：无 I/O，不 import gateway / runtime。
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass

from pydantic import BaseModel

from .models import Config


@dataclass(frozen=True, slots=True)
class SettingGroup:
    """一个业务分组（= 设置面板左列的一个锚点）。

    Attributes:
        id: 稳定标识（wire 上是 ``groups[].id``）；改名 / 调序都不该改它。
        title: 显示名（面板锚点、YAML 分隔行、``wing config list`` 的分组头）。
        doc: 一行说明（面板锚点的详情、YAML 分隔行下方的块注释）。
        members: 成员 = ``Config`` 的**顶层键名**（catalog root 直接子节点的 ``key``）。
            顺序不表达文件顺序（文件顺序恒为模型声明序）；它只用于「这一组包含什么」。
    """

    id: str
    title: str
    doc: str
    members: tuple[str, ...]


#: 分组表（**顺序即界面顺序**，前端不再自己排序）。
#:
#: 与 ``config.yaml`` 的顶层键无对应关系：一个组可以含多个键（``Behavior`` 四个），
#: 一个键只属于一个组（:func:`build_groups` 的门禁强制）。
SETTING_GROUPS: tuple[SettingGroup, ...] = (
    SettingGroup(
        id="providers",
        title="Providers",
        doc="LLM provider 与模型目录（目录只有一个来源：这里的声明）",
        members=("providers",),
    ),
    SettingGroup(
        id="agents",
        title="Agents",
        doc="Agent 模板：模型引用 / 工具集 / 提示词 / skills 与 rules",
        members=("agents",),
    ),
    SettingGroup(
        id="behavior",
        title="Behavior",
        doc="Agent 行为与内置工具的通用开关（bash 安全 / 结果截断）",
        members=("safe_command_patterns", "yolo", "steer", "tool_result_truncate"),
    ),
    SettingGroup(
        id="images",
        title="Images",
        doc="ReadImage 与请求期图片投影（预算按请求生效）",
        members=("images",),
    ),
    SettingGroup(
        id="sessions",
        title="Sessions",
        doc="会话内存态回收（磁盘状态一概不动）",
        members=("sessions",),
    ),
    SettingGroup(
        id="gateway",
        title="Gateway",
        doc="网关监听 / 鉴权 / 远程工具",
        members=("gateway",),
    ),
    SettingGroup(
        id="advanced",
        title="Advanced",
        doc="扩展点（hooks / prompt 命令）· 日志 · 低层少用开关",
        members=("hooks", "commands", "log", "user_agent"),
    ),
)


def group_of(
    key: str, groups: Sequence[SettingGroup] = SETTING_GROUPS
) -> SettingGroup | None:
    """一个顶层键属于哪个组；没有归属返回 ``None``（调用方决定是容忍还是报错）。"""
    for group in groups:
        if key in group.members:
            return group
    return None


def validate_groups(groups: Sequence[SettingGroup], fields: Sequence[str]) -> None:
    """校验一张分组表对一组顶层字段的**覆盖**（违反即 ``ValueError``）。

    悄悄漏掉一个顶层键会让它在界面里无处可去，所以这里是硬失败。四条：

    1. 组 ``id`` / ``title`` 不重复（id 是稳定标识，title 是界面文案，两者都要唯一）；
    2. 组不为空（没有成员的组是一个点不开的锚点）；
    3. 每个成员都是真实存在的顶层字段，且只属于一个组；
    4. 每个顶层字段都被某个组认领。
    """
    seen_ids: set[str] = set()
    seen_titles: set[str] = set()
    seen_members: dict[str, str] = {}
    known = set(fields)
    for group in groups:
        if group.id in seen_ids:
            raise ValueError(f"设置分组 id 重复：{group.id}")
        if group.title in seen_titles:
            raise ValueError(f"设置分组 title 重复：{group.title}")
        seen_ids.add(group.id)
        seen_titles.add(group.title)
        if not group.members:
            raise ValueError(f"设置分组 {group.id} 没有成员")
        for member in group.members:
            if member not in known:
                raise ValueError(f"设置分组 {group.id} 的成员不是顶层字段：{member}")
            if member in seen_members:
                raise ValueError(
                    f"顶层字段 {member} 同时属于 {seen_members[member]} 与 {group.id}"
                )
            seen_members[member] = group.id
    missing = known - set(seen_members)
    if missing:
        raise ValueError(f"顶层字段没有归入任何设置分组：{sorted(missing)}")


def build_groups(
    root: type[BaseModel] = Config, groups: Sequence[SettingGroup] = SETTING_GROUPS
) -> list[SettingGroup]:
    """分组表（列表形态，**顺序即界面顺序**）+ 覆盖校验（:func:`validate_groups`）。

    ``root`` 不是 :class:`Config` 时（单测的手工小模型）返回空表：分组是 ``Config``
    这张表的属性，不是任意模型的。``groups`` 参数只为单测注入坏表（生产路径恒为默认）。
    """
    if root is not Config:
        return []
    validate_groups(groups, list(Config.model_fields))
    return list(groups)


__all__ = [
    "SETTING_GROUPS",
    "SettingGroup",
    "build_groups",
    "group_of",
    "validate_groups",
]
