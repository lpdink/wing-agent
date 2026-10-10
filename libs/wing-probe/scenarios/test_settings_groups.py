"""设置目录的**业务分组**（``GET /api/settings/schema`` 的 ``groups``）。

分组是「配置在文件里的存储形式」与「界面里的分类」之间的那道缝：设置面板左列的锚点
只认这张表（前端零硬编码），``wing config list`` 的分组头与 ``config.yaml`` 的
``# ── Name ───`` 分隔行也来自它。它一旦漂移，界面就会把设置项归到错误的锚点下
（或者干脆无处可去）——而这类漂移在单测里看不见，只有真网关的 wire 响应能抓住。

覆盖的断言点：

- **顺序与成员**：7 个组的 id / title 顺序固定（Providers → Advanced），成员两两不相交、
  合起来恰好等于 root 的全部直接子节点（漏一个 = 它在界面里无处可去）；
- **同源**：每个顶层节点的 ``section`` == 它所属组的 ``title``，``section_doc`` 只在组的
  声明序首成员上；嵌套节点的 ``section`` 恒 null（分组只属于顶层）；
- **零迁移**：既有顶层键一个没少、一个没改名（分组合并只发生在界面层）；
- **保存回归**：改两个「并组后同属 Advanced / Behavior」的键 → ``ok=true``、``changed``
  精确、文件仍可解析、顶层键集合不变，且分隔行跟着新分组走
  （``# ── Advanced ─`` 在，``Extensibility`` / ``Logging`` 不再出现）。
"""

from __future__ import annotations

from typing import Any

import pytest
import yaml

from wing_probe import Probe

#: 界面顺序（左列锚点）——改这里等于改产品决策。
EXPECTED_GROUPS: list[tuple[str, str, list[str]]] = [
    ("providers", "Providers", ["providers"]),
    ("agents", "Agents", ["agents"]),
    (
        "behavior",
        "Behavior",
        ["safe_command_patterns", "yolo", "steer", "tool_result_truncate"],
    ),
    ("images", "Images", ["images"]),
    ("sessions", "Sessions", ["sessions"]),
    ("gateway", "Gateway", ["gateway"]),
    ("advanced", "Advanced", ["hooks", "commands", "log", "user_agent"]),
]

#: 零迁移红线：``config.yaml`` 的顶层键（分组怎么并都不该动它们）。
TOP_LEVEL_KEYS = [member for _, _, members in EXPECTED_GROUPS for member in members]


async def _schema(http: Any) -> dict:
    return await http.request("GET", "/api/settings/schema")


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_schema_groups_are_the_navigation_anchors(probe: Probe) -> None:
    """groups 的顺序 / 成员 / 与目录 section 的同源关系。"""
    http = probe.driver_required.http
    schema = await _schema(http)
    groups = schema["groups"]

    assert [(g["id"], g["title"], g["members"]) for g in groups] == EXPECTED_GROUPS, (
        groups
    )
    assert all(g["doc"].strip() for g in groups), groups

    # 成员两两不相交，且合起来恰好是 root 的全部直接子节点（顺序 = 声明序）。
    members = [member for group in groups for member in group["members"]]
    assert len(members) == len(set(members)), members
    children = [child["key"] for child in schema["root"]["children"]]
    assert sorted(members) == sorted(children), (members, children)

    # 同一份事实的两个投影：节点的 section == 所属组的 title；section_doc 只在首成员上。
    title_of = {
        member: group["title"] for group in groups for member in group["members"]
    }
    first_of: set[str] = set()
    for child in schema["root"]["children"]:
        group = next(g for g in groups if child["key"] in g["members"])
        assert child["section"] == title_of[child["key"]] == group["title"], child[
            "key"
        ]
        if group["id"] not in first_of:
            first_of.add(group["id"])
            assert child["section_doc"] == group["doc"], child["key"]
        else:
            assert child["section_doc"] is None, child["key"]

    # 嵌套字段没有分组（分组只属于顶层；两处都写 = 两份真相）。
    def walk(node: dict) -> None:
        for child in node.get("children") or []:
            assert child["section"] is None, child["path"]
            walk(child)
        if node.get("element") is not None:
            walk(node["element"])
        for variant in node.get("variants") or []:
            walk(variant)

    for child in schema["root"]["children"]:
        walk(child)


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_grouping_does_not_touch_the_stored_document(probe: Probe) -> None:
    """零迁移 + 保存回归：改两个跨旧分组的键，文件里的顶层键与路径一个不动。"""
    http = probe.driver_required.http
    config_path = probe.env.config_path

    # 基线：文件里的顶层键全部是已知键（probe 的 fixture 是稀疏文档，缺席 = 跟随默认）。
    before = yaml.safe_load(config_path.read_text(encoding="utf-8"))
    assert set(before) <= set(TOP_LEVEL_KEYS), sorted(before)

    current = await http.request("GET", "/api/settings/get")
    document = current["values"]
    # `log.level` 原属 Logging、`user_agent.preset` 原属 Advanced —— 现在同属 Advanced；
    # `yolo` 属 Behavior。三个都改一遍，确认并组没有影响寻址与保存。
    document.setdefault("log", {})["level"] = "WARNING"
    document.setdefault("user_agent", {})["preset"] = "qwen-code"
    document["yolo"] = True

    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["problems"] == [], receipt
    assert set(receipt["changed"]) == {"log.level", "user_agent.preset", "yolo"}, (
        receipt["changed"]
    )
    # 三个都是 hot / next_session 域：不需要重启。
    assert receipt["restart_required"] == [], receipt

    text = config_path.read_text(encoding="utf-8")
    after = yaml.safe_load(text)
    # 零迁移：保存不新增 / 不丢失任何存储键，键名也不变（分组只活在界面层）。
    assert set(after) <= set(TOP_LEVEL_KEYS), sorted(after)
    assert set(before) <= set(after), (sorted(before), sorted(after))
    assert after["log"]["level"] == "WARNING", after["log"]
    assert after["user_agent"]["preset"] == "qwen-code", after["user_agent"]
    assert after["yolo"] is True, after["yolo"]

    # 分隔行跟着新分组走（界面的分类同时是文件的分段依据，键序仍是声明序）。
    banners = [
        line[len("# ── ") :].split(" ")[0]
        for line in text.splitlines()
        if line.startswith("# ── ") and line.rstrip().endswith("─")
    ]
    assert banners == [title for _, title, _ in EXPECTED_GROUPS], banners

    # 保存之后 schema 的分组不变（它是静态声明，不随文档变）。
    again = await _schema(http)
    assert [g["id"] for g in again["groups"]] == [
        group_id for group_id, _, _ in EXPECTED_GROUPS
    ], again["groups"]
