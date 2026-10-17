# tests/test_config_emit.py
"""emit（``config/emit.py``）的单测 —— 规范形 YAML 的 round-trip 与输出规则。

覆盖面：

1. **round-trip**：``emit → yaml.safe_load`` 与输入稀疏文档逐项相等（含多级嵌套列表、空容器、
   多行字符串、非 ASCII、需要引号的值）；
2. **幂等**：``emit(load(emit(doc))) == emit(doc)``；
3. 注释只有两类：分组分隔行与注释掉的默认值；声明层的 ``doc`` / ``notes`` / ``example`` /
   生效域 / 密文说明一条都不进文件；
4. 缺席且有默认 → **注释掉的默认值**（列表项只展开第一项）；默认是 ``null`` → 不写；
   缺席且必填 → 不注释的空值；
5. 未知键保留（父容器末尾 + 标记注释，含「父容器是显式空对象」的边界）；
6. ``default_document()`` 的产出 = 首启模板（两个空列表 + 注释掉的默认值、无 ``ChangeHere``）；
7. **模板可用性**：喂给 ``Config(**raw)`` 只失败在「providers / agents 不得为空」这两条上。
"""

from __future__ import annotations

import re
from typing import Any, get_args, get_origin

import pytest
import yaml
from pydantic import BaseModel, create_model

from wing.config import (
    ApplyScope,
    Config,
    S,
    SettingKind,
    build_catalog,
    cross_field_problems,
    default_document,
    emit_config_yaml,
)
from wing.config.catalog import SettingNode
from wing.config.groups import build_groups
from wing.config.document import ExtraKey, read_document

# ─────────────────────────────────────────────────────────────────────────────
# fixtures
# ─────────────────────────────────────────────────────────────────────────────


@pytest.fixture(scope="module")
def catalog() -> SettingNode:
    return build_catalog()


@pytest.fixture(scope="module")
def template(catalog: SettingNode) -> str:
    return emit_config_yaml(default_document(), catalog)


@pytest.fixture(scope="module")
def provider_text(catalog: SettingNode) -> str:
    """一份带 provider / agent 的输出：``providers[]`` 的字段只有列表非空时才会被写出来。"""
    return emit_config_yaml(
        {
            "providers": [
                {"name": "p", "base_url": "b", "api_key": "k", "models": ["m"]}
            ],
            "agents": [{"name": "a", "model": "m"}],
        },
        catalog,
    )


def _minimal_document() -> dict:
    """一份「合法配置」的稀疏文档（含字符串 / 对象 / 变体列表三种形态）。"""
    return {
        "providers": [
            {
                "name": "default",
                "protocol": "anthropic",
                "base_url": "https://api.anthropic.com",
                "api_key": "sk-ant-xxx",
                "models": [
                    "claude-sonnet-4-20250514",
                    {
                        "id": "sonnet",
                        "name": "claude-sonnet-4-20250514",
                        "display_name": "Claude Sonnet 4",
                        "capabilities": {"vision": True},
                    },
                ],
                "extra_body": {"thinking": {"type": "enabled", "budget_tokens": 4096}},
            }
        ],
        "agents": [
            {
                "name": "default",
                "model": "claude-sonnet-4-20250514",
                "tools": ["Bash", "Read", "ReadImage"],
                "skills": ["~/.agents/skills/*/SKILL.md"],
            }
        ],
    }


def _declared_nodes(node: SettingNode):
    """声明节点（不含 root / 元素模板 / 变体）：缺省的顶层与嵌套字段。"""
    if node.path != "config" and not node.path.endswith("[]"):
        yield node
    for child in node.children:
        yield from _declared_nodes(child)


# ─────────────────────────────────────────────────────────────────────────────
# 1 / 2. round-trip 与幂等
# ─────────────────────────────────────────────────────────────────────────────


def test_round_trip_of_a_realistic_document(
    catalog: SettingNode,
) -> None:
    doc = _minimal_document()
    text = emit_config_yaml(doc, catalog)
    assert yaml.safe_load(text) == doc


def test_round_trip_covers_hard_scalar_shapes(catalog: SettingNode) -> None:
    """需要引号 / 转义 / 多行 / 非 ASCII / 空串 / 纯数字 / true-like 的值。"""
    needles = [
        "a: b",
        "#comment",
        "- item",
        "123",
        "true",
        "null",
        "2001-12-14",
        "",
        "多行\n文本",
        "  leading",
        "trailing  ",
        "a # b",
        "中文 and ascii",
        "quote'and\"double",
        "tab\there",
    ]
    doc = {"providers": [], "agents": [], "safe_command_patterns": needles}
    text = emit_config_yaml(doc, catalog)
    assert yaml.safe_load(text) == doc


def test_round_trip_covers_empty_and_null(catalog: SettingNode) -> None:
    doc = {
        "providers": [{"name": "p", "base_url": "b", "api_key": "k", "models": ["m"]}],
        "agents": [{"name": "a", "model": "m"}],
        "gateway": {"auth": {"keys": []}},
        "hooks": [],
        "safe_command_patterns": [],
        "images": {"max_bytes": 1000000},
        "log": {"level": None},
    }
    text = emit_config_yaml(doc, catalog)
    assert yaml.safe_load(text) == doc
    # 空容器必须显式写（`key:` 后面只有注释 = YAML null）
    assert "keys: []" in text
    assert re.search(r"(?m)^hooks: \[\]$", text)
    assert re.search(r"(?m)^  level: null$", text)


def test_emit_is_idempotent(catalog: SettingNode) -> None:
    doc = _minimal_document()
    first = emit_config_yaml(doc, catalog)
    second = emit_config_yaml(yaml.safe_load(first), catalog)
    assert first == second


def test_empty_document_falls_back_to_required_empties(
    catalog: SettingNode, template: str
) -> None:
    assert yaml.safe_load(emit_config_yaml({}, catalog)) == {
        "providers": [],
        "agents": [],
    }
    assert emit_config_yaml({}, catalog) == template


# ─────────────────────────────────────────────────────────────────────────────
# 3. 文件里只有两类注释：分组分隔行 + 注释掉的默认值
#
# 字段的 doc / notes / example / 生效域 / 密文说明归设置面板与 `wing config` 所有
# （同一个声明层），文件只承载数据与可发现性——散文注释会把几十行的配置撑成上千行。
# ─────────────────────────────────────────────────────────────────────────────

_COMMENT_MARKERS = ("e.g. ", "生效：", "密钥：")


def test_no_prose_comments_from_the_declarations(
    catalog: SettingNode, template: str, provider_text: str
) -> None:
    """声明层的文案一个都不许进文件（反查全部 doc / notes，不是抽查几句）。"""
    prose: list[str] = []
    for node in _declared_nodes(catalog):
        if node.doc:
            prose.append(node.doc)
        prose.extend(node.notes)
        if node.example:
            prose.append(f"e.g. {node.example}")
    assert len(prose) > 50, "声明层的文案是这份测试的前提，读不到说明遍历坏了"
    for text in (template, provider_text):
        for line in prose:
            assert line not in text, line
        for marker in _COMMENT_MARKERS:
            assert marker not in text, marker


def test_every_comment_line_is_a_banner_or_a_keyed_value(template: str) -> None:
    """注释行只有三种形态：文件头那一行、`# ── Name ──…` 分隔行、一条 `key:` 行。"""
    lines = template.splitlines()
    assert lines[0].startswith("# wing-agent configuration")
    for line in lines[1:]:
        if not line.lstrip().startswith("#"):
            continue
        stripped = line.lstrip()
        assert stripped.startswith("# ── ") or ":" in stripped, line


def test_section_banners_have_no_section_doc(template: str) -> None:
    assert "# ── Providers ─" in template
    assert "# ── Advanced ─" in template
    lines = template.splitlines()
    banner = lines.index(
        next(line for line in lines if line.startswith("# ── Sessions"))
    )
    assert lines[banner + 1] == "# sessions:"  # 分隔行之后直接是键，没有分组说明
    # 分隔行宽度固定（旧模板的观感）
    assert len(lines[banner]) == 64


def test_empty_section_drops_its_banner() -> None:
    """正文为空的分组连分隔行一起省掉（一条没有内容的横幅只会让人以为文件被截断）。"""
    mini = create_model(
        "_M",
        a=(bool, S(doc="开关", default=True)),
        b=(str | None, S(doc="可空（默认 null → 不写）", default=None)),
    )
    catalog = build_catalog(mini)
    catalog.children[0].section = "First"
    catalog.children[1].section = "Second"
    text = emit_config_yaml({}, catalog)
    assert "# ── First" in text
    assert "# ── Second" not in text, text
    assert "# a: true" in text


def test_section_banners_are_exactly_the_group_table(template: str) -> None:
    """分隔行 = 分组表（顺序、名字、每组恰好一次）；文件里的键序仍是声明序。

    分组表管**界面**分类，emitter 只是把它当作分段依据——所以「并组」在文件里的
    唯一可见后果是少几行分隔注释，键一个都不动（零迁移）。
    """
    banners = re.findall(r"(?m)^# ── (.+?) ─+$", template)
    assert banners == [group.title for group in build_groups()], banners


def test_header_is_one_line_without_timestamp(template: str) -> None:
    lines = template.splitlines()
    assert lines[0] == "# wing-agent configuration — $WING_HOME/core/config.yaml"
    assert lines[1] == ""
    # 不写生成时间戳（会让每次保存都产生 diff 噪声）；文件头里不许出现日期形态
    assert not re.search(r"\d{4}-\d{2}-\d{2}", lines[0])


# ─────────────────────────────────────────────────────────────────────────────
# 4. 缺席 → 注释掉的默认值 / 必填 → 空值
# ─────────────────────────────────────────────────────────────────────────────


def test_absent_default_is_commented_out(provider_text: str, template: str) -> None:
    assert "# timeout_first_chunk: 300.0" in provider_text
    assert not re.search(r"(?m)^\s*timeout_first_chunk:", provider_text)
    assert "# yolo: false" in template
    assert "# safe_command_patterns: []" in template


def test_absent_null_default_is_not_written(provider_text: str, template: str) -> None:
    """默认是 ``null`` 的键连注释行都不写：``null`` 没有可展示的事实。"""
    for needle in ("description", "max_turns", "image_delivery"):
        assert f"# {needle}: null" not in provider_text, needle
        assert f"# {needle}: null" not in template, needle


def test_absent_object_subtree_is_commented_out(template: str) -> None:
    assert re.search(r"(?m)^# gateway:$", template)
    assert re.search(r"(?m)^  # host: 127\.0\.0\.1$", template)
    assert re.search(r"(?m)^    # enabled: false$", template)
    # 注释块里不能出现任何真实键（否则会把默认值钉进文件）
    loaded = yaml.safe_load(template)
    assert loaded == {"providers": [], "agents": []}


def test_object_default_without_showable_children_degrades_to_empty_map() -> None:
    """子树里没有一个可展示的默认值 ⇒ 写 ``{}``（裸 `key:` 取消注释是 null）。"""
    inner = create_model("_Inner", note=(str | None, S(doc="注", default=None)))
    mini = create_model(
        "_Outer", box=(inner, S(doc="盒子", default_factory=lambda: inner()))
    )
    text = emit_config_yaml({}, build_catalog(mini))
    assert "# box: {}" in text
    assert yaml.safe_load(text) in (None, {})


def test_absent_required_is_written_as_an_empty_value(catalog: SettingNode) -> None:
    doc = {"providers": [{"name": "p"}], "agents": [{"name": "a", "model": "m"}]}
    text = emit_config_yaml(doc, catalog)
    assert '    base_url: ""' in text
    assert '    api_key: ""' in text
    # models 有默认值（[]）→ 缺席时是注释行；它仍会在跨字段检查里报「没有模型」
    assert "# models: []" in text
    problems = cross_field_problems(Config.model_construct(**yaml.safe_load(text)))
    assert [(problem.path, problem.kind.value) for problem in problems] == [
        ("providers[0].models", "missing_required")
    ]


def test_present_object_keeps_commented_defaults_inside(catalog: SettingNode) -> None:
    doc = {
        "providers": [{"name": "p", "base_url": "b", "api_key": "k", "models": ["m"]}],
        "agents": [{"name": "a", "model": "m"}],
        "gateway": {"port": 39999},
    }
    text = emit_config_yaml(doc, catalog)
    assert re.search(r"(?m)^gateway:$", text)
    assert re.search(r"(?m)^  port: 39999$", text)
    assert re.search(r"(?m)^  # host: 127\.0\.0\.1$", text)
    assert yaml.safe_load(text) == doc


def test_empty_object_item_is_written_explicitly(catalog: SettingNode) -> None:
    doc = {
        "providers": [
            {},
            {"name": "p", "base_url": "b", "api_key": "k", "models": ["m"]},
        ],
        "agents": [{"name": "a", "model": "m"}],
    }
    text = emit_config_yaml(doc, catalog)
    assert "  - {}" in text
    assert yaml.safe_load(text) == doc


# ─────────────────────────────────────────────────────────────────────────────
# 5. 列表项：只展开第一项的缺席默认值
# ─────────────────────────────────────────────────────────────────────────────


def test_list_items_expand_defaults_only_for_the_first_item(
    catalog: SettingNode,
) -> None:
    """条目级开关一次可发现（第一项），又不随条目数膨胀（第 2..N 项只写用户写下的键）。"""
    provider = {
        "name": "p",
        "base_url": "b",
        "api_key": "k",
        "models": ["m"],
    }
    doc = {
        "providers": [dict(provider, name="p1"), dict(provider, name="p2")],
        "agents": [{"name": "a1", "model": "m"}, {"name": "a2", "model": "m"}],
    }
    text = emit_config_yaml(doc, catalog)
    first, second = text.split("  - name: p2", 1)
    assert "# protocol: openai" in first
    assert "# protocol: openai" not in second
    assert "# max_retries: 10" in first and "# max_retries: 10" not in second
    agents = text.split("  - name: a1", 1)[1]
    head, tail = agents.split("  - name: a2", 1)
    assert "# default: false" in head
    assert "# default: false" not in tail
    assert yaml.safe_load(text) == doc


def test_nested_first_item_expansion_follows_the_outer_list(
    catalog: SettingNode,
) -> None:
    """``allow_defaults`` 沿首项链条传下去：第 2 个 provider 的第 1 个 model 也不展开。"""
    doc = {
        "providers": [
            {"name": "p1", "base_url": "b", "api_key": "k", "models": ["m"]},
            {"name": "p2", "base_url": "b", "api_key": "k", "models": [{"name": "m2"}]},
        ],
        "agents": [{"name": "a", "model": "m"}],
    }
    text = emit_config_yaml(doc, catalog)
    tail = text.split("  - name: p2", 1)[1]
    assert "# capabilities:" not in tail
    assert yaml.safe_load(text) == doc


# ─────────────────────────────────────────────────────────────────────────────
# 6. 未知键保留
# ─────────────────────────────────────────────────────────────────────────────


def test_unknown_keys_are_written_back_at_the_parent_tail(
    catalog: SettingNode,
) -> None:
    doc = {"providers": [], "agents": [], "gateway": {"port": 1, "future_knob": True}}
    text = emit_config_yaml(
        doc, catalog, extra=[ExtraKey("gateway", "future_knob", True)]
    )
    index = text.index("future_knob: true")
    marker = text.rindex(
        "# unknown key (not recognized by this wing version)", 0, index
    )
    assert marker < index  # 标记注释在上方
    assert text.index("remote_tool_timeout") < marker  # 父容器已知键之后
    assert yaml.safe_load(text) == doc


def test_unknown_root_key_is_written_at_the_file_tail(catalog: SettingNode) -> None:
    doc = {"providers": [], "agents": [], "wat": {"a": [1, 2]}}
    text = emit_config_yaml(doc, catalog, extra=[ExtraKey("", "wat", {"a": [1, 2]})])
    assert text.rstrip().endswith(("- 2", "2"))
    assert "# unknown key (not recognized by this wing version)" in text
    assert yaml.safe_load(text) == doc


def test_unknown_key_under_an_explicitly_empty_parent_stays_in_place(
    catalog: SettingNode,
) -> None:
    """父容器是**用户显式写下的空对象**时，未知键也必须留在容器里（不是甩到文件末尾）。

    真实路径：磁盘上写 `gateway:\n  weird: 1` → `_split_known` 把它拆成
    `data["gateway"] = {}` + `extra=("gateway", ...)`。空对象若直接写成 `gateway: {}`，
    那个 `{}` 会吞掉位置信息，`weird` 就变成顶层键了。
    """
    doc = {
        "providers": [{"name": "p", "base_url": "b", "api_key": "k", "models": ["m"]}],
        "agents": [{"name": "a", "model": "m"}],
        "gateway": {},
    }
    text = emit_config_yaml(doc, catalog, extra=[ExtraKey("gateway", "weird", 1)])
    loaded = yaml.safe_load(text)
    assert loaded["gateway"] == {"weird": 1}, loaded
    assert "weird" not in loaded, loaded


def test_unknown_key_without_its_parent_is_still_preserved(
    catalog: SettingNode,
) -> None:
    """父容器缺席（输入不一致）：条目兜底写回文件末尾，绝不丢。"""
    text = emit_config_yaml(
        {"providers": [], "agents": []}, catalog, extra=[ExtraKey("nowhere", "knob", 7)]
    )
    assert "knob: 7" in text


def test_single_line_mapping_unknown_key_stays_valid_yaml(
    catalog: SettingNode,
) -> None:
    """未知键的值是**单键映射**时，dump 出来的单行仍是块结构（`future: x`）。

    直接拼在 `key: ` 后面会产出非法 YAML（`key: future: x`）——前向兼容承诺
    「原样写回」，写出一份解析不了的文件等于把它弄丢了。03 的端到端保存路径
    （``test_gateway_settings.py::test_set_keeps_unknown_keys``）先撞上这个形态。
    """
    extra = [
        ExtraKey("", "future_section", {"future_key": "keep-me"}),
        ExtraKey("nowhere", "one", [1]),
    ]
    doc = {"providers": [], "agents": []}
    text = emit_config_yaml(doc, catalog, extra=extra)
    loaded = yaml.safe_load(text)
    assert loaded["future_section"] == {"future_key": "keep-me"}
    assert loaded["one"] == [1]
    assert "future_section: future_key" not in text


# ─────────────────────────────────────────────────────────────────────────────
# B1 回归（Rework r1）：容器载荷不许被当成标量内联
#
# `safe_dump` 给出单行的**非空容器**（`a: 1` / `- 1`）是块结构的首行，不是标量；
# 拼在 `key: ` 后面会产出 `key: a: 1` 这种解析不了的文件（数据损坏级，AD7）。
# ─────────────────────────────────────────────────────────────────────────────


def test_single_key_map_payload_is_emitted_as_a_block(catalog: SettingNode) -> None:
    """B1 的原始复现（生产路径①）：``extra_body`` 写单键平铺 map。"""
    doc = {
        "providers": [
            {
                "name": "p",
                "base_url": "http://x",
                "api_key": "k",
                "models": ["m"],
                "extra_body": {"top_p": 0.9},
            }
        ],
        "agents": [{"name": "a", "model": "m"}],
    }
    text = emit_config_yaml(doc, catalog)
    assert "extra_body:\n      top_p: 0.9\n" in text  # 块风格，键独占一行
    assert "extra_body: top_p" not in text
    assert yaml.safe_load(text) == doc


def test_single_element_list_payload_is_emitted_as_a_block(
    catalog: SettingNode,
) -> None:
    """B1 的另一半（生产路径②）：未知键的值是**单元素 list**。"""
    doc = {"providers": [], "agents": []}
    text = emit_config_yaml(doc, catalog, extra=[ExtraKey("", "future_knob", [1])])
    assert "future_knob:\n  - 1\n" in text
    assert "future_knob: - 1" not in text
    assert yaml.safe_load(text)["future_knob"] == [1]


def test_scalar_and_empty_container_payloads_stay_inline(catalog: SettingNode) -> None:
    """内联只对**标量**与**空容器**成立（它们 dump 出来的单行才是真·单行）。"""
    extra = [
        ExtraKey("", "a_null", None),
        ExtraKey("", "a_bool", True),
        ExtraKey("", "a_int", 7),
        ExtraKey("", "a_str", "x"),
        ExtraKey("", "a_map", {}),
        ExtraKey("", "a_list", []),
    ]
    text = emit_config_yaml({"providers": [], "agents": []}, catalog, extra=extra)
    for line in (
        "a_null: null",
        "a_bool: true",
        "a_int: 7",
        "a_str: x",
        "a_map: {}",
        "a_list: []",
    ):
        assert line in text
    loaded = yaml.safe_load(text)
    assert [loaded[entry.key] for entry in extra] == [None, True, 7, "x", {}, []]


def test_multi_line_container_payload_keeps_its_block_shape(
    catalog: SettingNode,
) -> None:
    """多行容器的既有行为不回归：键独占一行，载荷缩进 +2。"""
    payload = {"a": [1, 2], "b": {"c": 3}}
    text = emit_config_yaml(
        {"providers": [], "agents": []}, catalog, extra=[ExtraKey("", "wat", payload)]
    )
    lines = text.splitlines()
    index = lines.index("wat:")
    assert lines[index + 1].startswith("  a:")
    assert yaml.safe_load(text)["wat"] == payload


def test_commented_container_default_keeps_the_block_shape() -> None:
    """注释态同形：缺席 map 的非空默认值整块注释，注释产物仍是合法 YAML。"""
    mini = create_model(
        "_M",
        a_map=(
            dict,
            S(doc="映射", apply=ApplyScope.HOT, default_factory=lambda: {"a": 1}),
        ),
    )
    text = emit_config_yaml({}, build_catalog(mini))
    assert "# a_map:\n  # a: 1\n" in text
    assert (yaml.safe_load(text) or {}) == {}


# ─────────────────────────────────────────────────────────────────────────────
# A2 回归：键名本身含点（或别的 YAML 边界形态）的未知键
#
# 失败形态：`gateway: {foo.bar: 42}` 保存一次后变成顶层
# `bar: 42`——`_Extras` 曾用 `rpartition(".")` 从"路径字符串"反推父容器，键名里的点
# 被当成分隔符。修复把 extra 改成结构性三元组（父前缀 + 原始键名 + 值）。
# 参数化即键名 fuzz 的物化：位置、名字、值三者都必须不变。
# ─────────────────────────────────────────────────────────────────────────────

#: 边界键名（含点 / YAML 歧义形态 / 空串）。33 个字符串的 fuzz 不在仓库里，
#: 这里固化成门禁；未来再加边界字符串就加进这批。
_BOUNDARY_UNKNOWN_KEYS = [
    # 含点：本回归的靶子
    "foo.bar",
    "gateway.foo.bar",  # 名字长得像一条完整父路径
    "a.b.c",
    "providers[0].x",  # 名字里有规范路径的写法
    "trailing.",
    ".leading",
    ".",
    "..",
    "...",
    "a..b",
    "键.点",
    # YAML 解析歧义：类型词 / 结构字符 / 引号形态
    "true",
    "false",
    "null",
    "~",
    "123",
    "1.5",
    "-",
    "- item",
    "#comment",
    "[bracket]",
    "{brace}",
    "*alias",
    "&anchor",
    "!tag",
    "|pipe",
    ">fold",
    "%percent",
    "@at",
    "`tick`",
    '"quote"',
    "'quote'",
    "back\\slash",
    "with space",
    "tab\tkey",
    "nl\nkey",
    "",
]

_VALID_BODY: dict[str, Any] = {
    "providers": [
        {"name": "p", "base_url": "https://x", "api_key": "k", "models": ["m"]}
    ],
    "agents": [{"name": "default", "model": "m"}],
}


def _config_with_unknown(position: str, key: str, payload: Any) -> dict[str, Any]:
    """一份合法配置 + 在指定位置塞入一个未知键（三种位置：顶层 / 嵌套对象 / 列表项）。"""
    import copy

    doc = copy.deepcopy(_VALID_BODY)
    if position == "root":
        doc[key] = payload
    elif position == "nested":
        doc["gateway"] = {"port": 40000, key: payload}
    else:  # list_item
        doc["providers"][0][key] = payload
    return doc


@pytest.fixture
def config_path(tmp_path, monkeypatch) -> Any:
    """把 ``document`` 的读取口钉到 tmp 文件（与 test_config_document 同一手法）。"""
    path = tmp_path / "core" / "config.yaml"
    path.parent.mkdir(parents=True)
    monkeypatch.setattr("wing.config.document.get_config_path", lambda: path)
    return path


@pytest.mark.parametrize("position", ["root", "nested", "list_item"])
@pytest.mark.parametrize("key", _BOUNDARY_UNKNOWN_KEYS)
@pytest.mark.parametrize(
    "payload", [7, {"inner": 1}, [1, 2]], ids=["scalar", "map", "list"]
)
def test_boundary_unknown_key_round_trips_in_place(
    position: str, key: str, payload: Any, config_path: Any, catalog: SettingNode
) -> None:
    """读盘 → emit → safe_load：未知键的**位置、名字、值**三者都不变（A2 的全面回归）。"""
    expected = _config_with_unknown(position, key, payload)
    config_path.write_text(
        yaml.safe_dump(expected, allow_unicode=True, sort_keys=False),
        encoding="utf-8",
    )
    doc, _ = read_document()
    text = emit_config_yaml(doc.data, catalog, extra=doc.extra)
    loaded = yaml.safe_load(text)
    assert loaded == expected  # 一处不等就红（挪位 / 改名 / 改值 / 吃键）
    # 定位断言（失败时给出比"两份大 dict 不等"更指向性的证据）
    if position == "root":
        assert loaded[key] == payload
    elif position == "nested":
        assert loaded["gateway"][key] == payload
    else:
        assert loaded["providers"][0][key] == payload


def test_dotted_unknown_key_stays_under_its_parent(config_path: Any) -> None:
    """A2 的原始复现：位置 / 名字都不许被 `rpartition` 挪走。"""
    config_path.write_text(
        "gateway:\n"
        "  port: 40000\n"
        "  foo.bar: 42\n"
        "providers:\n"
        "  - name: p\n"
        "    base_url: https://x\n"
        "    api_key: k\n"
        "    models: [m]\n"
        "    a.b: dotkey\n"
        "agents: [{name: default, model: m}]\n",
        encoding="utf-8",
    )
    doc, _ = read_document()
    text = emit_config_yaml(doc.data, build_catalog(), extra=doc.extra)
    loaded = yaml.safe_load(text)
    assert loaded["gateway"]["foo.bar"] == 42
    assert loaded["providers"][0]["a.b"] == "dotkey"
    assert "bar" not in loaded  # 旧形态：顶层 `bar: 42`
    assert "b" not in loaded["providers"][0]  # 旧形态：`b: dotkey` 落在 provider 里


def test_list_item_nested_sequence_payload_keeps_depth() -> None:
    """没有元素形态声明的 list（防御路径）：嵌套序列项不多包一层。

    这条路径用紧凑形态（`- - 1`）就够——它正是 ``safe_dump([[1]])`` 自己的写法
    （变异验证：改成"dash 独占一行 + 整体缩进"解析结果不变，所以这里不是 B1 的同族缺陷）。
    """
    bare = SettingNode(
        key="bare", path="bare", title="bare", doc="", kind=SettingKind.LIST
    )
    root = SettingNode(
        key="config",
        path="config",
        title="config",
        doc="",
        kind=SettingKind.OBJECT,
        children=[bare],
    )
    text = emit_config_yaml({"bare": [[1], [2, 3]]}, root)
    assert "  - - 1" in text
    assert yaml.safe_load(text) == {"bare": [[1], [2, 3]]}


def _walk_fields(model: type[BaseModel], prefix: str = "") -> list[tuple[str, Any]]:
    """``(路径, FieldInfo)``：递归走 `model` 可达的全部声明字段（含 union / 列表元素）。"""
    out: list[tuple[str, Any]] = []
    for name, field in model.model_fields.items():
        path = f"{prefix}.{name}" if prefix else name
        out.append((path, field))
        for child in (field.annotation, *get_args(field.annotation)):
            if isinstance(child, type) and issubclass(child, BaseModel):
                out.extend(_walk_fields(child, path))
    return out


def test_every_list_declaration_defaults_to_empty() -> None:
    """S2 门禁：emitter 对 list 的缺席值写死 `[]`（catalog 的 `default=None` 是结构口径）。

    这条门禁让那个「发明值」保持诚实：所有非必填 LIST 声明的实际 factory 默认都必须是空列表。
    未来某个 list 带非空默认时，模板会静默撒谎——这里先红。
    """
    lists = [
        (path, field)
        for path, field in _walk_fields(Config)
        if get_origin(field.annotation) is list
    ]
    assert sorted(path for path, _ in lists) == [
        "agents",
        "agents.rules",
        "agents.skills",
        "agents.tools",
        "commands.paths",
        "gateway.auth.keys",
        "gateway.cors.origins",
        "hooks",
        "providers",
        "providers.models",
        "safe_command_patterns",
    ]
    for path, field in lists:
        assert (
            field.is_required() or field.get_default(call_default_factory=True) == []
        ), path


def test_element_template_fields_are_rendered_when_items_exist(
    catalog: SettingNode,
) -> None:
    """S2：元素模板字段只有列表非空时才出现在文件里——它们必须真的被渲染过。

    `image_delivery` / `image_max_bytes`（provider 层）与 `vision`（对象形态模型的
    capabilities）是旧模板点名要可见的三个字段，空模板不展开元素，所以单独看一眼。
    """
    doc = {
        "providers": [
            {
                "name": "p",
                "base_url": "b",
                "api_key": "k",
                "models": [{"name": "m"}],
            }
        ],
        "agents": [{"name": "a", "model": "m"}],
    }
    text = emit_config_yaml(doc, catalog)
    # 对象形态的默认子树照常展开（`# capabilities:` → `# vision: false`）；
    # 默认是 null 的元素字段（image_delivery / image_max_bytes）没有可展示的事实。
    assert re.search(r"(?m)^\s*# capabilities:$", text)
    assert re.search(r"(?m)^\s*# vision: false$", text)
    assert "# image_delivery: null" not in text
    assert "# image_max_bytes: null" not in text


# ─────────────────────────────────────────────────────────────────────────────
# 7. 首启模板
# ─────────────────────────────────────────────────────────────────────────────


def test_default_document_shape() -> None:
    assert default_document() == {"providers": [], "agents": []}
    assert default_document() is not default_document()  # 每次新 dict（调用方可以改）


def test_default_template_is_the_first_file_a_user_sees(template: str) -> None:
    assert yaml.safe_load(template) == default_document()
    assert "ChangeHere" not in template
    assert template.startswith("# wing-agent configuration —")


def test_default_template_covers_every_declared_field(
    catalog: SettingNode, template: str
) -> None:
    """emitter 的覆盖性守门：可展示的声明字段都以值行或注释行的形式出现。

    旧版守的是「手写模板与 models.py 的 SYNC」；模板由声明生成后，这份纪律死亡——
    守门改为「emitter 不许漏掉任何有值可展示的字段」。默认是 ``null`` 的标量没有可展示
    的事实（不写），其余一律出现（在首项里展开）。
    """
    for node in _declared_nodes(catalog):
        if node.kind is SettingKind.OBJECT or node.kind is SettingKind.LIST:
            showable = True
        else:
            showable = node.required or (node.has_default and node.default is not None)
        if not showable:
            continue
        assert f"# {node.key}:" in template or f"{node.key}:" in template, node.path


def test_default_template_carries_no_documentation(
    catalog: SettingNode, template: str
) -> None:
    """每个被展开的字段的 ``doc`` / ``notes`` 都不许出现在文件里（文档归面板与 CLI）。"""
    checked = 0
    for node in _declared_nodes(catalog):
        if not node.doc:
            continue
        checked += 1
        assert node.doc not in template, node.path
        for note in node.notes:
            assert note not in template, node.path
    assert checked > 20  # 声明层确实有文案（否则本测试是空转）


# ─────────────────────────────────────────────────────────────────────────────
# 8. 模板可用性：只失败在「两个空列表」
# ─────────────────────────────────────────────────────────────────────────────


def test_template_only_fails_on_empty_providers_and_agents(template: str) -> None:
    raw = yaml.safe_load(template)
    with pytest.raises(ValueError, match="agents list cannot be empty"):
        Config(**raw)
    problems = cross_field_problems(Config.model_construct(**raw))
    assert [(problem.path, problem.kind.value) for problem in problems] == [
        ("agents", "empty_list"),
        ("providers", "empty_list"),
    ]


def test_emitter_works_on_a_hand_made_catalog() -> None:
    """emitter 与 build_catalog 都可独立单测（不需要真实 Config 的 68 个字段）。"""

    inner = create_model(
        "_Inner", flag=(bool, S(doc="布尔", apply=ApplyScope.HOT, default=True))
    )
    mini = create_model(
        "_Mini",
        title=(str, S(doc="标题", apply=ApplyScope.HOT, default="hi")),
        nested=(inner, S(doc="嵌套", apply=ApplyScope.HOT, default_factory=inner)),
        items=(
            list[str],
            S(doc="列表", apply=ApplyScope.HOT, default_factory=list, min_items=1),
        ),
    )

    small = build_catalog(mini)
    text = emit_config_yaml({"nested": {"flag": False}, "items": ["x"]}, small)
    assert yaml.safe_load(text) == {"nested": {"flag": False}, "items": ["x"]}
    assert "# title: hi" in text
    assert "# 标题" not in text
