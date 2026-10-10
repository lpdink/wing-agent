# tests/test_config_groups.py
"""业务分组门禁 —— 界面分类的唯一声明处（``wing/config/groups.py``）。

分组是「配置在文件里的存储形式」与「界面里的分类」之间的那道解耦缝：``config.yaml``
的顶层键不动，加组 / 并组 / 改名 / 调序只动 ``SETTING_GROUPS`` 这一张表。这里钉住三件事：

1. **表本身**：id / title 唯一、成员非空、顺序即界面顺序（前端不再自己排序）；
2. **覆盖**：每个顶层字段恰好属于一个组（漏一个 = 它在界面里无处可去；重复 = 两份真相），
   且成员都是真实存在的字段（改名 / 删字段忘了同步 = 幻影成员）；
3. **投影**：``build_catalog()`` 把组盖到 root 的直接子节点上（``section`` = title、
   ``section_doc`` 只在声明序首成员上），嵌套字段一律没有分组；``build_groups()`` 的
   wire 形状（id / title / doc / members）与表的顺序一致。

外加一条**回归红线**：既有顶层键一个都没被改名 / 挪走（零迁移的承诺）。
"""

from __future__ import annotations

import pytest

from wing.config import Config
from wing.config.catalog import build_catalog
from wing.config.groups import SETTING_GROUPS, SettingGroup, build_groups, group_of
from wing.config.groups import validate_groups

#: 界面顺序（左列锚点的顺序）——改这里等于改产品决策，要过评审。
_EXPECTED_ORDER = [
    "providers",
    "agents",
    "behavior",
    "images",
    "sessions",
    "gateway",
    "advanced",
]

#: 分组成员表（顶层键 → 组 id）。既是覆盖门禁的期望值，也是「零迁移」的回归红线：
#: ``config.yaml`` 的顶层键与它们的归属都不许悄悄变。
_EXPECTED_MEMBERS = {
    "providers": "providers",
    "agents": "agents",
    "safe_command_patterns": "behavior",
    "yolo": "behavior",
    "steer": "behavior",
    "tool_result_truncate": "behavior",
    "images": "images",
    "sessions": "sessions",
    "gateway": "gateway",
    "hooks": "advanced",
    "commands": "advanced",
    "log": "advanced",
    "user_agent": "advanced",
}


# ─────────────────────────────────────────────────────────────────────────────
# 1. 表本身
# ─────────────────────────────────────────────────────────────────────────────


def test_group_order_is_the_interface_order() -> None:
    assert [group.id for group in SETTING_GROUPS] == _EXPECTED_ORDER


def test_group_ids_and_titles_are_unique() -> None:
    ids = [group.id for group in SETTING_GROUPS]
    titles = [group.title for group in SETTING_GROUPS]
    assert len(set(ids)) == len(ids), ids
    assert len(set(titles)) == len(titles), titles


def test_every_group_is_presentable() -> None:
    """锚点要能直接渲染：title / doc 非空、成员非空且不带路径（成员是顶层键名）。"""
    for group in SETTING_GROUPS:
        assert group.title.strip(), group.id
        assert group.doc.strip(), group.id
        assert group.members, group.id
        for member in group.members:
            assert "." not in member and "[" not in member, f"{group.id}: {member}"


# ─────────────────────────────────────────────────────────────────────────────
# 2. 覆盖（每个顶层键恰好一个组）
# ─────────────────────────────────────────────────────────────────────────────


def test_members_cover_every_top_level_field_exactly_once() -> None:
    membership = {
        member: group.id for group in SETTING_GROUPS for member in group.members
    }
    assert membership == _EXPECTED_MEMBERS
    assert set(membership) == set(Config.model_fields), (
        f"漏：{sorted(set(Config.model_fields) - set(membership))} "
        f"幻影：{sorted(set(membership) - set(Config.model_fields))}"
    )


def test_group_of_resolves_members_and_rejects_strangers() -> None:
    gateway = group_of("gateway")
    assert gateway is not None and gateway.id == "gateway"
    advanced = group_of("user_agent")
    assert advanced is not None and advanced.id == "advanced"
    assert group_of("nope") is None
    # 嵌套路径不是成员（成员只到顶层键）。
    assert group_of("gateway.port") is None


def test_build_groups_returns_the_table_in_order() -> None:
    groups = build_groups()
    assert [group.id for group in groups] == _EXPECTED_ORDER
    assert all(isinstance(group, SettingGroup) for group in groups)


def test_build_groups_is_empty_for_a_foreign_root() -> None:
    """分组是 ``Config`` 这张表的属性：手工小模型（单测用）没有分组。"""
    from pydantic import BaseModel

    from wing.config.spec import ApplyScope, S

    class Tiny(BaseModel):
        x: int = S(doc="手工小模型的字段", apply=ApplyScope.HOT, default=0)

    assert build_groups(Tiny) == []
    assert [child.section for child in build_catalog(Tiny).children] == [None]


# ─────────────────────────────────────────────────────────────────────────────
# 3. 校验函数（坏表必须硬失败，而不是悄悄漏一个键）
# ─────────────────────────────────────────────────────────────────────────────

_FIELDS = ["a", "b", "c"]


def _group(
    group_id: str, members: tuple[str, ...], title: str | None = None
) -> SettingGroup:
    return SettingGroup(
        id=group_id,
        title=title or group_id.title(),
        doc=f"{group_id} 的说明",
        members=members,
    )


def test_validate_groups_accepts_a_complete_partition() -> None:
    validate_groups(
        [_group("one", ("a", "b")), _group("two", ("c",))],
        _FIELDS,
    )


@pytest.mark.parametrize(
    ("groups", "fragment"),
    [
        ([_group("x", ("a",)), _group("x", ("b", "c"))], "id 重复"),
        (
            [_group("x", ("a",), title="Same"), _group("y", ("b", "c"), title="Same")],
            "title 重复",
        ),
        ([_group("x", ("a",)), _group("y", ())], "没有成员"),
        ([_group("x", ("a", "nope")), _group("y", ("b", "c"))], "不是顶层字段"),
        ([_group("x", ("a", "b")), _group("y", ("b", "c"))], "同时属于"),
        ([_group("x", ("a", "b"))], "没有归入任何设置分组"),
        ([], "没有归入任何设置分组"),
    ],
)
def test_validate_groups_rejects_broken_tables(
    groups: list[SettingGroup], fragment: str
) -> None:
    with pytest.raises(ValueError, match=fragment):
        validate_groups(groups, _FIELDS)


def test_catalog_raises_when_a_top_level_field_has_no_group(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """目录构建期就炸（界面里无处可去的键不许悄悄消失）。"""
    from wing.config import catalog as catalog_module

    monkeypatch.setattr(catalog_module, "build_groups", lambda root=Config: [])
    with pytest.raises(ValueError, match="没有归入任何设置分组"):
        build_catalog()


# ─────────────────────────────────────────────────────────────────────────────
# 4. 目录投影（emitter / CLI / 旧客户端读的还是 section）
# ─────────────────────────────────────────────────────────────────────────────


def test_catalog_stamps_group_titles_on_top_level_children() -> None:
    by_id = {group.id: group for group in SETTING_GROUPS}
    catalog = build_catalog()
    assert [child.section for child in catalog.children] == [
        by_id[_EXPECTED_MEMBERS[child.key]].title for child in catalog.children
    ]
    for child in catalog.children:
        group = group_of(child.key)
        assert group is not None, child.path
        assert child.section == group.title, child.path


def test_group_doc_lands_on_the_first_declared_member_only() -> None:
    """``section_doc`` 是 emitter 的一条块注释：每组恰好一次，且在声明序首成员上。"""
    catalog = build_catalog()
    by_id = {group.id: group for group in SETTING_GROUPS}
    seen: set[str] = set()
    for child in catalog.children:
        group_id = _EXPECTED_MEMBERS[child.key]
        if group_id not in seen:
            seen.add(group_id)
            assert child.section_doc == by_id[group_id].doc, child.path
        else:
            assert child.section_doc is None, child.path
    assert seen == set(by_id)


def test_nested_fields_carry_no_group() -> None:
    """分组只属于 root 的直接子节点（嵌套字段带分组 = 两份真相）。"""
    catalog = build_catalog()

    def walk(node) -> None:  # noqa: ANN001
        for child in node.children:
            assert child.section is None, child.path
            assert child.section_doc is None, child.path
            walk(child)
        if node.element is not None:
            walk(node.element)
        for variant in node.variants or []:
            walk(variant)

    for child in catalog.children:
        walk(child)
