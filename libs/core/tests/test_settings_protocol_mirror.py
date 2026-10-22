"""``SettingNode`` ↔ ``SettingNodeProto`` 的字段集门禁。

领域节点与 wire 节点是**逐字段手写投影**（``gateway/projection.build_settings_node``）：
字段名即协议，显式清单有维护价值，但「加一个渲染提示字段要改三处、漏一处静默丢字段」
没有任何红灯。本文件补上那道红灯：

- 字段集必须满足 ``set(proto) == set(node) - {"identity_field"}``（后者是纯后端语义，
  刻意不投影）；
- 投影必须把**每一个** proto 字段都填上真值（不靠 pydantic 默认兜底），
  用 ``model_fields_set`` 断言「逐个显式赋值」。
"""

from __future__ import annotations

from wing.config import SettingNode, build_catalog
from wing.gateway.projection import build_settings_node
from wing.gateway.protocol import SettingNodeProto

#: 领域有、wire 刻意没有的字段（纯后端语义）。
NOT_ON_THE_WIRE = {"identity_field"}


def test_proto_field_set_matches_domain_field_set():
    assert (
        set(SettingNodeProto.model_fields)
        == set(SettingNode.model_fields) - NOT_ON_THE_WIRE
    ), (
        "SettingNodeProto 与 SettingNode 的字段集漂移了：加/删字段时两侧与 "
        "projection.build_settings_node 必须一起改"
    )


def test_projection_fills_every_field_explicitly():
    """投影逐字段赋值（不留 pydantic 默认兜底的空洞）。

    递归检查整棵树（含 ``children`` / ``element`` / ``variants``）：任一层漏填，
    该节点的 ``model_fields_set`` 就缺那一项。
    """
    proto_root = build_settings_node(build_catalog())
    walked = 0

    def walk(node: SettingNodeProto) -> None:
        nonlocal walked
        walked += 1
        assert set(node.model_fields_set) == set(SettingNodeProto.model_fields), (
            f"{node.path}: 投影漏填字段 "
            f"{sorted(set(SettingNodeProto.model_fields) - set(node.model_fields_set))}"
        )
        for child in node.children:
            walk(child)
        if node.element is not None:
            walk(node.element)
        for variant in node.variants or []:
            walk(variant)

    walk(proto_root)
    assert walked > 1, "目录树至少要有根 + 若干字段节点"
