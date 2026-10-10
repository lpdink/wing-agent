# tests/test_config_emit.py
"""emit（``config/emit.py``）的单测 —— 规范形 YAML 的 round-trip 与八条输出规则。

覆盖面（对应 02 design.md 的 D9–D13）：

1. **round-trip**：``emit → yaml.safe_load`` 与输入稀疏文档逐项相等（含多级嵌套列表、空容器、
   多行字符串、非 ASCII、需要引号的值）；
2. **幂等**：``emit(load(emit(doc))) == emit(doc)``；
3. 注释就位：``doc`` 在值上方、``notes`` 逐行、``example`` 追加、section 分隔行与 ``section_doc``；
4. 缺席且有默认 → **注释掉的默认值**；缺席且必填 → 不注释的空值；
5. ``apply`` 标记（restart / next_session 有，hot 没有）与 secret 说明行；
6. 未知键保留（父容器末尾 + 标记注释）；
7. ``default_document()`` 的产出 = 首启模板（两个空列表 + 全套注释、无 ``ChangeHere``）；
8. **模板可用性**：喂给 ``Config(**raw)`` 只失败在「providers / agents 不得为空」这两条上。
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


def _blocks(text: str) -> list[list[str]]:
    """按空行切块（注释与字段成组，便于断言"哪个注释挨着哪个值"）。"""
    blocks: list[list[str]] = [[]]
    for line in text.splitlines():
        if line.strip():
            blocks[-1].append(line)
        else:
            blocks.append([])
    return [block for block in blocks if block]


def _block_of(text: str, needle: str) -> list[str]:
    for block in _blocks(text):
        if any(needle in line for line in block):
            return block
    raise AssertionError(f"输出里没有包含 {needle!r} 的块")


def _has(block: list[str], needle: str) -> bool:
    return any(needle in line for line in block)


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
# 3. 注释来自声明
# ─────────────────────────────────────────────────────────────────────────────


def test_doc_and_notes_sit_above_the_value(provider_text: str) -> None:
    block = _block_of(provider_text, "timeout_first_chunk")
    assert _has(block, "# 流式首块超时（秒）")
    assert _has(block, "# 这是**响应头**超时")
    # 注释在值上方（不用行尾注释）
    index = next(i for i, line in enumerate(block) if "timeout_first_chunk" in line)
    assert block[index].lstrip().startswith("#")


def test_example_line_is_appended(provider_text: str) -> None:
    block = _block_of(provider_text, "base_url: b")
    assert _has(block, "# e.g. https://api.openai.com/v1")


def test_section_banner_and_section_doc(template: str) -> None:
    assert "# ── Providers ─" in template
    assert "# ── Advanced ─" in template
    lines = template.splitlines()
    banner = lines.index(
        next(line for line in lines if line.startswith("# ── Sessions"))
    )
    assert lines[banner + 1] == "# 会话内存态回收（磁盘状态一概不动）"
    # 分隔行宽度固定（旧模板的观感）
    assert len(lines[banner]) == 64


def test_section_banners_are_exactly_the_group_table(template: str) -> None:
    """分隔行 = 分组表（顺序、名字、每组恰好一次）；文件里的键序仍是声明序。

    分组表管**界面**分类，emitter 只是把它当作分段依据——所以「并组」在文件里的
    唯一可见后果是少几行分隔注释，键一个都不动（零迁移）。
    """
    banners = re.findall(r"(?m)^# ── (.+?) ─+$", template)
    assert banners == [group.title for group in build_groups()], banners


def test_header_has_no_timestamp(template: str) -> None:
    header = template.splitlines()[:4]
    assert header[0].startswith("# ──")
    assert header[1] == "# wing-agent configuration"
    assert "$WING_HOME/core/config.yaml" in header[2]
    assert "/settings" in header[3]
    # 不写生成时间戳（会让每次保存都产生 diff 噪声）；文件头里不许出现日期形态
    assert not re.search(r"\d{4}-\d{2}-\d{2}", "\n".join(header))


# ─────────────────────────────────────────────────────────────────────────────
# 4. 缺席 → 注释掉的默认值 / 必填 → 空值
# ─────────────────────────────────────────────────────────────────────────────


def test_absent_default_is_commented_out(provider_text: str, template: str) -> None:
    assert "# timeout_first_chunk: 300.0" in provider_text
    assert not re.search(r"(?m)^\s*timeout_first_chunk:", provider_text)
    assert "# yolo: false" in template
    assert "# safe_command_patterns: []" in template


def test_absent_object_subtree_is_commented_out(template: str) -> None:
    assert re.search(r"(?m)^# gateway:$", template)
    assert re.search(r"(?m)^  # host: 127\.0\.0\.1$", template)
    assert re.search(r"(?m)^    # enabled: false$", template)
    # 注释块里不能出现任何真实键（否则会把默认值钉进文件）
    loaded = yaml.safe_load(template)
    assert loaded == {"providers": [], "agents": []}


def test_absent_required_is_written_as_an_empty_value(catalog: SettingNode) -> None:
    doc = {"providers": [{"name": "p"}], "agents": [{"name": "a", "model": "m"}]}
    text = emit_config_yaml(doc, catalog)
    assert "# 服务端点" in text
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
# 5. 生效域标记与密文说明
# ─────────────────────────────────────────────────────────────────────────────


def test_apply_markers(template: str, provider_text: str) -> None:
    port_block = _block_of(template, "# port: 32523")
    assert _has(port_block, "# 生效：需重启网关")
    assert "# 生效：新会话" in provider_text
    # hot 是默认档，不加噪声：紧邻的注释窗口里没有生效域标记
    assert "# 生效：热重载" not in template
    lines = provider_text.splitlines()
    index = next(i for i, line in enumerate(lines) if "timeout_first_chunk" in line)
    assert not any("生效" in line for line in lines[max(0, index - 3) : index])


def test_secret_note(provider_text: str) -> None:
    block = _block_of(provider_text, "api_key: k")
    assert _has(block, "# 密钥：面板里只写不回显（末 4 位提示）")
    index = next(i for i, line in enumerate(block) if "api_key: k" in line)
    assert "密钥" in block[index - 1]


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
# 失败形态（审查 A2 的原始复现）：`gateway: {foo.bar: 42}` 保存一次后变成顶层
# `bar: 42`——`_Extras` 曾用 `rpartition(".")` 从"路径字符串"反推父容器，键名里的点
# 被当成分隔符。修复把 extra 改成结构性三元组（父前缀 + 原始键名 + 值）。
# 这一批参数化即审查者临时 fuzz 的物化：位置、名字、值三者都必须不变。
# ─────────────────────────────────────────────────────────────────────────────

#: 边界键名（含点 / YAML 歧义形态 / 空串）。审查者的 33 字符串 fuzz 不在仓库里，
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
    """A2 的原始复现（审查者给的两例）：位置 / 名字都不许被 `rpartition` 挪走。"""
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
    assert re.search(r"(?m)^\s*# image_delivery: null$", text)
    assert re.search(r"(?m)^\s*# image_max_bytes: null$", text)
    assert re.search(r"(?m)^\s*# vision: false$", text)


# ─────────────────────────────────────────────────────────────────────────────
# 7. 首启模板
# ─────────────────────────────────────────────────────────────────────────────


def test_default_document_shape() -> None:
    assert default_document() == {"providers": [], "agents": []}
    assert default_document() is not default_document()  # 每次新 dict（调用方可以改）


def test_default_template_is_the_first_file_a_user_sees(template: str) -> None:
    assert yaml.safe_load(template) == default_document()
    assert "ChangeHere" not in template
    assert "# 推荐在 TUI 里用 /settings 编辑" in template


def test_default_template_covers_every_declared_field(
    catalog: SettingNode, template: str
) -> None:
    """emitter 的覆盖性守门：每个声明字段都以值行或注释行的形式出现。

    旧版守的是「手写模板与 models.py 的 SYNC」；模板由声明生成后，这份纪律死亡——
    守门改为「emitter 不许漏掉任何字段」。
    """
    for node in _declared_nodes(catalog):
        assert f"# {node.key}:" in template or f"{node.key}:" in template, node.path


def test_default_template_loses_no_documentation(
    catalog: SettingNode, template: str
) -> None:
    """每个被展开的字段（root 的非元素后代）的 ``doc`` / ``notes`` 都出现在文件里。"""
    for node in _declared_nodes(catalog):
        assert node.doc in template, node.path
        for note in node.notes:
            assert note in template, node.path


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
    assert "# 标题" in text
    assert "# title: hi" in text
