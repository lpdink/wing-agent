"""`GET /api/settings/schema` 的目录契约 —— 面板与 `wing config` 的全部素材来源。

目录是 01（声明层）/ 02（catalog）对外的唯一投影，它一旦漂移，前端拿到的
kind / apply / choices / min_items / variants 就全错位——而单测只覆盖到 Python
对象，覆盖不到 **wire 形状**。本场景按真实响应建路径索引后抽查关键路径：

- 根节点 ``key == path == "config"``（增补 P2：两种拼写不共存，后端固定一种）；
- **密文**：``providers[].api_key`` / ``gateway.auth.keys[].key`` 都是
  ``secret=true`` 且 ``kind == "secret"``（面板据此走密文编辑器）；
- **生效域**：``gateway.port`` / ``gateway.host`` / ``sessions.eviction.sweep_interval_seconds``
  是 ``restart``，``log.level`` / ``images.max_images`` / ``providers[].extra_body`` 是 ``hot``
  （回执的 ``restart_required`` 判定就靠它）；
- **不得为空**：``providers`` / ``agents`` / ``providers[].models`` 的 ``min_items == 1``
  （与跨字段检查同一条事实的机器可读镜像）；
- **union**：``providers[].models`` 的 ``variants`` 是两个形态（裸字符串 / ModelSpec 对象），
  且对象形态可达 ``id`` / ``capabilities.vision``；
- **P1 修正**：``summary_fields`` 是**数组不是 null**（Rust 侧是 ``Vec<String>``，
  发 null 会让反序列化失败）；``value_hint`` 后端恒发 null（只有 Rust 侧 Interface 根发 "color"）；
- 顶层 ``version`` 非空、``config_path`` 是绝对路径（面板标题栏与 `wing config` 展示）。
"""

from __future__ import annotations

from collections.abc import Iterator
from typing import Any

import pytest

from wing_probe import Probe

#: 抽查表：规范路径 → 该节点的部分字段期望（真实响应形状，见 `docs/dev/probe-testing.md`）。
PATH_EXPECTATIONS: dict[str, dict[str, Any]] = {
    # ── 根与两个列表容器（"不得为空"） ──
    "config": {"kind": "object", "apply": "restart"},
    "providers": {
        "kind": "list",
        "apply": "next_session",
        "required": True,
        "min_items": 1,
    },
    "providers[]": {"kind": "object"},
    "agents": {
        "kind": "list",
        "apply": "next_session",
        "required": True,
        "min_items": 1,
    },
    # ── provider 字段：必填 / 密文 / 枚举 / 透传 map ──
    "providers[].name": {"kind": "str", "apply": "next_session", "required": True},
    "providers[].protocol": {
        "kind": "enum",
        "apply": "hot",
        "choices": ["openai", "anthropic"],
    },
    "providers[].base_url": {"kind": "str", "apply": "hot", "required": True},
    "providers[].api_key": {
        "kind": "secret",
        "secret": True,
        "apply": "hot",
        "required": True,
    },
    "providers[].extra_body": {"kind": "map", "apply": "hot"},
    "providers[].reasoning_effort": {
        "kind": "enum",
        "apply": "hot",
        "nullable": True,
        "choices": ["low", "medium", "high", "max"],
    },
    # ── 模型声明（union） ──
    "providers[].models": {"kind": "list", "apply": "hot", "min_items": 1},
    "providers[].models[].id": {"kind": "str", "nullable": True},
    "providers[].models[].name": {"kind": "str"},
    "providers[].models[].capabilities.vision": {"kind": "bool"},
    # ── agent 模板 ──
    "agents[].name": {"kind": "str", "apply": "next_session", "required": True},
    "agents[].model": {"kind": "str", "apply": "next_session", "required": True},
    "agents[].tools": {"kind": "list", "apply": "next_session"},
    "agents[].max_turns": {"kind": "int", "nullable": True},
    # ── 网关（restart 域的两个代表） ──
    "gateway.host": {"kind": "str", "apply": "restart"},
    "gateway.port": {"kind": "int", "apply": "restart"},
    "gateway.auth.keys[].key": {"kind": "secret", "secret": True, "required": True},
    "gateway.auth.keys[].role": {"kind": "enum", "choices": ["admin", "tool_runtime"]},
    # ── 逐出：同一节里 hot 与 restart 并存（容器 apply 只是最粗一档，权威在叶子） ──
    "sessions.eviction.idle_ttl_seconds": {"kind": "float", "apply": "hot"},
    "sessions.eviction.sweep_interval_seconds": {"kind": "float", "apply": "restart"},
    # ── 其它热点字段 ──
    "images.max_images": {"kind": "int", "apply": "hot"},
    "log.level": {
        "kind": "enum",
        "apply": "hot",
        "choices": ["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"],
    },
    "commands.paths": {"kind": "list", "apply": "hot"},
    "hooks": {"kind": "list", "apply": "hot"},
    "yolo": {"kind": "bool", "apply": "next_session"},
    "safe_command_patterns": {"kind": "list", "apply": "hot"},
}


def _walk(root: dict[str, Any]) -> Iterator[dict[str, Any]]:
    """深度遍历目录（children / element / variants）产出每个节点。

    ``variants`` 的元素（union 候选形态）与 ``element`` 的模板**共享同一个 path**
    （如两个 ``providers[].models[]``），所以索引按 ``path -> [node, …]`` 建。
    """
    stack = [root]
    while stack:
        node = stack.pop()
        yield node
        stack.extend(node.get("children") or [])
        if node.get("element") is not None:
            stack.append(node["element"])
        stack.extend(node.get("variants") or [])


def _index(root: dict[str, Any]) -> dict[str, list[dict[str, Any]]]:
    index: dict[str, list[dict[str, Any]]] = {}
    for node in _walk(root):
        index.setdefault(node["path"], []).append(node)
    return index


def _only(index: dict[str, list[dict[str, Any]]], path: str) -> dict[str, Any]:
    """路径唯一命中的节点（list 元素的 union 变体除外——那两个用 variants 断言）。"""
    nodes = index.get(path)
    assert nodes, (
        f"{path!r} is missing from the settings schema; known paths: {sorted(index)}"
    )
    assert len(nodes) == 1, (path, [node["kind"] for node in nodes])
    return nodes[0]


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_schema_pins_the_key_paths(probe: Probe) -> None:
    """schema 的 wire 形状：抽查 29 条关键路径 + 三条全局不变量。"""
    http = probe.driver_required.http
    schema = await http.request("GET", "/api/settings/schema")

    # ── 顶层三件套 ──
    assert isinstance(schema["version"], str) and schema["version"], schema["version"]
    config_path = schema["config_path"]
    assert config_path.startswith("/") and config_path.endswith("config.yaml"), (
        config_path
    )
    assert config_path == str(probe.env.config_path), config_path

    index = _index(schema["root"])

    # ── 根节点：P2 的拼写（key 与 path 都恒为 "config"） ──
    root = schema["root"]
    assert (root["key"], root["path"]) == ("config", "config"), (
        root["key"],
        root["path"],
    )
    assert root["children"], root

    # ── 抽查表 ──
    for path, expected in PATH_EXPECTATIONS.items():
        node = _only(index, path)
        for field, want in expected.items():
            actual = node[field]
            if field == "choices":
                # wire 形状是 [{value, doc}] —— 与声明层的 {value: doc} 同构。
                actual = [choice["value"] for choice in actual]
            assert actual == want, (path, field, actual, want)

    # ── union：providers[].models 的两个候选形态（裸字符串 / ModelSpec 对象） ──
    models = _only(index, "providers[].models")
    variants = models["variants"]
    assert [variant["kind"] for variant in variants] == ["str", "object"], variants
    object_variant = variants[1]
    child_paths = {child["path"] for child in object_variant["children"]}
    assert "providers[].models[].id" in child_paths, child_paths
    assert "providers[].models[].capabilities" in child_paths, child_paths
    assert models["summary_fields"] == ["id", "name", "display_name"], models
    assert _only(index, "providers")["summary_fields"] == [
        "name",
        "protocol",
        "base_url",
    ], index["providers"]

    # ── P1 修正：`summary_fields` 是数组（Rust Vec<String>），绝不发 null；
    #    `value_hint` 后端恒 null（"color" 只由 Rust 侧的 Interface 根声明）。
    #    枚举选项必须带解释（面板内联展开要展示它）。 ──
    for node in _walk(schema["root"]):
        assert isinstance(node["summary_fields"], list), node["path"]
        assert node["value_hint"] is None, node["path"]
        for choice in node["choices"]:
            assert isinstance(choice["doc"], str) and choice["doc"], (
                node["path"],
                choice,
            )

    # ── 所有 path 都是规范地址（非空、根前缀正确） ──
    for node in _walk(schema["root"]):
        assert node["path"], node
    assert len(index) >= 70, len(index)
