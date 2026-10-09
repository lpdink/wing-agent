# tests/test_config_catalog.py
"""catalog（``config/catalog.py``）的单测 —— 类型推导全表 / 路径 / 约束 / 分组 / 解析器。

覆盖面（对应 02 design.md 的 D1–D6）：

1. 类型推导规则表逐行（含 ``bool`` 早于 ``int``、``Literal`` → enum、``choices`` 非空 ⇒ enum
   （增补 P10）、union → variants、``X | None`` → nullable、``dict`` → map、``BaseModel`` → object）；
2. path 生成（含多级列表嵌套与根拼写 = ``config``）；
3. 约束抽取（``gt``/``ge``/``lt``/``le`` → min/max + exclusive 标志、``pattern``、``min_items``）；
4. ``required`` / ``default`` / ``has_default``（含 default_factory 的 object/list 与 map 两种情况）；
5. section 分组与 ``order``（= 声明序）、合成节点的元信息；
6. ``parse_path`` 全用例（合法 5 例 + 非法 12 例，文法与 Rust 侧同批）；
7. **规模断言**：声明字段总数 = 68（01 报告的迁移字段数），防「某个嵌套模型被漏扫」。
"""

from __future__ import annotations

from typing import Literal

import pytest
from pydantic import create_model

from wing.config import S, ApplyScope
from wing.config.catalog import (
    ELEMENT_KEY,
    ROOT_KEY,
    Element,
    Index,
    Key,
    SettingKind,
    SettingNode,
    build_catalog,
    parse_path,
)
from wing.config.models import Config

# ─────────────────────────────────────────────────────────────────────────────
# 手工小模型：类型推导全表（不依赖 Config 的现状，改配置不会让这些用例失真）
# ─────────────────────────────────────────────────────────────────────────────


def _kind_table() -> dict[str, SettingNode]:
    """一张「注解形态 → kind」的最小模型，覆盖 §5.3 规则表的每一行。

    用 ``create_model`` 声明（字段名只是 dict 键）：vulture 不会把类体里的声明
    误当「没人用的类变量」，而这个文件里没有需要被别处引用的模型。
    """
    inner = create_model(
        "_Inner",
        flag=(bool, S(doc="布尔", apply=ApplyScope.HOT, default=True)),
    )
    kinds = create_model(
        "_Kinds",
        a_bool=(bool, S(doc="先判布尔", apply=ApplyScope.HOT, default=False)),
        an_int=(int, S(doc="整数", apply=ApplyScope.HOT, default=1, gt=0)),
        a_float=(float, S(doc="浮点", apply=ApplyScope.HOT, default=1.5, ge=0, lt=10)),
        a_literal=(
            Literal["x", "y"],
            S(
                doc="字面量",
                apply=ApplyScope.HOT,
                default="x",
                choices={"x": "X 的含义", "y": "Y 的含义"},
            ),
        ),
        a_literal_bare=(
            Literal["p", "q"],
            S(doc="字面量（无含义声明）", apply=ApplyScope.HOT, default="p"),
        ),
        a_str_choices=(
            str,
            S(
                doc="裸 str + choices",
                apply=ApplyScope.HOT,
                default="s1",
                choices={"s1": "一", "s2": "二"},
            ),
        ),
        a_secret=(str, S(doc="密文", apply=ApplyScope.HOT, secret=True)),
        a_str=(str, S(doc="普通字符串", apply=ApplyScope.HOT, default="")),
        a_map=(dict, S(doc="自由映射", apply=ApplyScope.HOT, default_factory=dict)),
        a_object=(
            inner,
            S(doc="嵌套对象", apply=ApplyScope.HOT, default_factory=inner),
        ),
        a_list=(
            list[str],
            S(doc="标量列表", apply=ApplyScope.HOT, default_factory=list),
        ),
        a_union_list=(
            list[str | int],
            S(doc="联合列表", apply=ApplyScope.HOT, default_factory=list),
        ),
        optional_int=(
            int | None,
            S(doc="可空整数", apply=ApplyScope.HOT, default=None),
        ),
        optional_choices=(
            str | None,
            S(doc="可空枚举", apply=ApplyScope.HOT, default=None, choices={"k": "键"}),
        ),
    )
    root = build_catalog(kinds)
    assert root.kind is SettingKind.OBJECT
    return {child.key: child for child in root.children}


@pytest.fixture(scope="module")
def kinds() -> dict[str, SettingNode]:
    return _kind_table()


# ─────────────────────────────────────────────────────────────────────────────
# 遍历工具
# ─────────────────────────────────────────────────────────────────────────────


def _walk(node: SettingNode):
    """深度优先产出全部节点（含合成节点）。"""
    yield node
    if node.element is not None:
        yield from _walk(node.element)
    for variant in node.variants or []:
        yield from _walk(variant)
    for child in node.children:
        yield from _walk(child)


def _declared(node: SettingNode) -> list[SettingNode]:
    """声明节点（不含 root / 元素模板 / 变体）：每个字段恰好一个。"""
    return [
        item
        for item in _walk(node)
        if item.path != ROOT_KEY and not item.path.endswith("[]")
    ]


def _by_path(catalog: SettingNode, path: str) -> SettingNode:
    for item in _walk(catalog):
        if item.path == path:
            return item
    raise AssertionError(f"catalog 里没有 {path}")


# ─────────────────────────────────────────────────────────────────────────────
# 1. 类型推导全表
# ─────────────────────────────────────────────────────────────────────────────


def test_kind_table(kinds: dict[str, SettingNode]) -> None:
    expected = {
        "a_bool": SettingKind.BOOL,
        "an_int": SettingKind.INT,
        "a_float": SettingKind.FLOAT,
        "a_literal": SettingKind.ENUM,
        "a_literal_bare": SettingKind.ENUM,
        "a_str_choices": SettingKind.ENUM,
        "a_secret": SettingKind.SECRET,
        "a_str": SettingKind.STR,
        "a_map": SettingKind.MAP,
        "a_object": SettingKind.OBJECT,
        "a_list": SettingKind.LIST,
        "a_union_list": SettingKind.LIST,
        "optional_int": SettingKind.INT,
        "optional_choices": SettingKind.ENUM,
    }
    assert {name: kinds[name].kind for name in expected} == expected


def test_bool_is_judged_before_int(kinds: dict[str, SettingNode]) -> None:
    """``bool`` 是 ``int`` 的子类：顺序错了全部布尔字段会变成 int。"""
    assert kinds["a_bool"].kind is SettingKind.BOOL
    assert kinds["a_bool"].default is False


def test_literal_choices_take_args_and_meanings_from_declaration(
    kinds: dict[str, SettingNode],
) -> None:
    literal = kinds["a_literal"]
    assert [(choice.value, choice.doc) for choice in literal.choices] == [
        ("x", "X 的含义"),
        ("y", "Y 的含义"),
    ]
    bare = kinds["a_literal_bare"]
    assert [(choice.value, choice.doc) for choice in bare.choices] == [
        ("p", None),
        ("q", None),
    ]


def test_choices_on_bare_str_yield_enum(kinds: dict[str, SettingNode]) -> None:
    """增补 P10：值域只写在 ``choices`` 里的裸 ``str`` 也是 enum（面板给内联选择器）。"""
    node = kinds["a_str_choices"]
    assert node.kind is SettingKind.ENUM
    assert node.nullable is False
    assert [(choice.value, choice.doc) for choice in node.choices] == [
        ("s1", "一"),
        ("s2", "二"),
    ]


def test_optional_choices_are_enum_and_nullable(kinds: dict[str, SettingNode]) -> None:
    node = kinds["optional_choices"]
    assert node.kind is SettingKind.ENUM
    assert node.nullable is True
    assert [choice.value for choice in node.choices] == ["k"]


def test_optional_is_stripped_to_nullable_leaf(kinds: dict[str, SettingNode]) -> None:
    node = kinds["optional_int"]
    assert node.kind is SettingKind.INT
    assert node.nullable is True
    assert node.has_default is True
    assert node.default is None


def test_dict_is_map_and_model_is_object(kinds: dict[str, SettingNode]) -> None:
    assert kinds["a_map"].kind is SettingKind.MAP
    assert kinds["a_map"].has_default is True
    assert kinds["a_map"].default == {}
    assert kinds["a_object"].kind is SettingKind.OBJECT
    assert [child.key for child in kinds["a_object"].children] == ["flag"]
    assert kinds["a_object"].children[0].path == "a_object.flag"


def test_list_element_template(kinds: dict[str, SettingNode]) -> None:
    node = kinds["a_list"]
    assert node.kind is SettingKind.LIST
    assert node.variants is None
    assert node.element is not None
    assert node.element.kind is SettingKind.STR
    assert node.element.key == ELEMENT_KEY
    assert node.element.path == "a_list[]"
    assert node.element.order == 0


def test_union_list_yields_variants(kinds: dict[str, SettingNode]) -> None:
    node = kinds["a_union_list"]
    assert node.kind is SettingKind.LIST
    assert node.element is None
    assert node.variants is not None
    assert [variant.kind for variant in node.variants] == [
        SettingKind.STR,
        SettingKind.INT,
    ]
    assert [variant.order for variant in node.variants] == [0, 1]
    # 两个变体共享同一个模板路径（形态由文档里的实际值决定）
    assert {variant.path for variant in node.variants} == {"a_union_list[]"}


# ─────────────────────────────────────────────────────────────────────────────
# 2 / 3. 约束抽取
# ─────────────────────────────────────────────────────────────────────────────


def test_number_bounds(kinds: dict[str, SettingNode]) -> None:
    lower = kinds["an_int"]
    assert (lower.min, lower.max, lower.exclusive_min, lower.exclusive_max) == (
        0.0,
        None,
        True,
        False,
    )
    interval = kinds["a_float"]
    assert (interval.min, interval.max) == (0.0, 10.0)
    assert interval.exclusive_min is False
    assert interval.exclusive_max is True


def test_constraints_from_real_catalog() -> None:
    catalog = build_catalog()
    name = _by_path(catalog, "providers[].name")
    assert name.pattern == r"^[a-zA-Z0-9_-]+$"
    assert name.apply is ApplyScope.NEXT_SESSION
    max_bytes = _by_path(catalog, "images.max_bytes")
    assert (max_bytes.min, max_bytes.exclusive_min) == (0.0, True)
    assert max_bytes.default == 4_718_592
    assert _by_path(catalog, "providers[].models").min_items == 1
    assert _by_path(catalog, "providers[].timeout_first_chunk").min_length is None


# ─────────────────────────────────────────────────────────────────────────────
# 4. required / default / has_default
# ─────────────────────────────────────────────────────────────────────────────


def test_required_and_defaults_from_real_catalog() -> None:
    catalog = build_catalog()
    providers = _by_path(catalog, "providers")
    assert (providers.required, providers.has_default, providers.default) == (
        True,
        False,
        None,
    )
    assert _by_path(catalog, "agents").required is True
    timeout = _by_path(catalog, "providers[].timeout_first_chunk")
    assert (timeout.has_default, timeout.default) == (True, 300.0)
    hooks = _by_path(catalog, "hooks")
    assert (hooks.has_default, hooks.default) == (True, None)
    assert _by_path(catalog, "sessions").has_default is True
    assert _by_path(catalog, "sessions").default is None
    api_key = _by_path(catalog, "providers[].api_key")
    assert (api_key.required, api_key.has_default, api_key.secret) == (
        True,
        False,
        True,
    )


# ─────────────────────────────────────────────────────────────────────────────
# 5. 分组 / 声明序 / 合成节点元信息
# ─────────────────────────────────────────────────────────────────────────────


def test_root_spelling_and_children_order() -> None:
    catalog = build_catalog()
    assert (catalog.key, catalog.path) == (ROOT_KEY, "config")
    assert catalog.kind is SettingKind.OBJECT
    assert [child.key for child in catalog.children] == list(Config.model_fields)
    assert [child.order for child in catalog.children] == list(
        range(len(catalog.children))
    )


def test_sections_are_contiguous_and_documented() -> None:
    catalog = build_catalog()
    sections: list[str] = []
    for child in catalog.children:
        assert child.section is not None, child.path
        if child.section not in sections:
            sections.append(child.section)
    assert sections == [
        "Providers",
        "Agents",
        "Behavior",
        "Images",
        "Sessions",
        "Gateway",
        "Extensibility",
        "Logging",
        "Advanced",
    ]
    for section in sections:
        members = [c for c in catalog.children if c.section == section]
        assert members[0].section_doc is not None, section
        assert all(member.section_doc is None for member in members[1:]), section


def test_nested_fields_have_no_section() -> None:
    """``section`` 只属于 root 的直接子节点（嵌套字段带分组 = 两份真相）。"""
    for node in _declared(build_catalog()):
        if "." in node.path:
            assert node.section is None, node.path


def test_synthetic_nodes_have_minimal_metadata() -> None:
    catalog = build_catalog()
    for node in _walk(catalog):
        if node.path == ROOT_KEY or node.path.endswith("[]"):
            assert node.value_hint is None
            assert node.section is None
            assert node.section_doc is None
            assert node.deprecated is None
    element = _by_path(catalog, "providers[]")
    assert element.key == ELEMENT_KEY
    assert element.title == ELEMENT_KEY
    assert element.doc == ""
    assert element.notes == []
    assert element.apply is ApplyScope.NEXT_SESSION  # 子树最粗（A5 的同一约定）


def test_summary_fields_and_value_hint() -> None:
    """A1：``summary_fields`` 是**数组**（缺省 ``[]``，绝不发 ``None``）；``value_hint`` 恒 None。"""
    catalog = build_catalog()
    assert _by_path(catalog, "providers").summary_fields == [
        "name",
        "protocol",
        "base_url",
    ]
    assert _by_path(catalog, "agents").summary_fields == ["name", "model"]
    assert _by_path(catalog, "providers[].models").summary_fields == [
        "id",
        "name",
        "display_name",
    ]
    for node in _declared(catalog):
        assert isinstance(node.summary_fields, list), node.path
        if node.path not in (
            "providers",
            "agents",
            "providers[].models",
        ):
            assert node.summary_fields == [], node.path
        assert node.value_hint is None, node.path


def test_enum_fields_from_real_catalog() -> None:
    catalog = build_catalog()
    effort = _by_path(catalog, "providers[].reasoning_effort")
    assert effort.kind is SettingKind.ENUM  # A2（P10）：choices ⇒ enum
    assert effort.nullable is True
    assert [choice.value for choice in effort.choices] == [
        "low",
        "medium",
        "high",
        "max",
    ]
    assert all(choice.doc for choice in effort.choices)
    assert _by_path(catalog, "log.level").kind is SettingKind.ENUM
    assert _by_path(catalog, "providers[].protocol").kind is SettingKind.ENUM
    assert _by_path(catalog, "providers[].image_delivery").nullable is True


def test_nested_list_path_from_real_catalog() -> None:
    """多级列表嵌套的路径拼写：``providers[].models[].capabilities.vision``。"""
    catalog = build_catalog()
    vision = _by_path(catalog, "providers[].models[].capabilities.vision")
    assert vision.kind is SettingKind.BOOL
    assert vision.default is False
    models = _by_path(catalog, "providers[].models")
    assert models.element is None
    assert models.variants is not None
    assert [variant.kind for variant in models.variants] == [
        SettingKind.STR,
        SettingKind.OBJECT,
    ]
    # 对象变体自带子树（路径 = 元素模板路径 + 子字段名）
    assert [child.path for child in models.variants[1].children] == [
        "providers[].models[].id",
        "providers[].models[].name",
        "providers[].models[].display_name",
        "providers[].models[].description",
        "providers[].models[].capabilities",
    ]


def test_images_defaults_are_pinned() -> None:
    """旧模板测试的两个钉子：``max_bytes``（见 test_constraints_from_real_catalog）
    与 ``max_images``——后者原先无人接（S2），这里连同同段的其它默认值一起钉住。"""
    catalog = build_catalog()
    assert _by_path(catalog, "images.max_bytes").default == 4_718_592
    assert _by_path(catalog, "images.max_images").default == 32
    assert _by_path(catalog, "images.count_quantum").default == 8
    assert _by_path(catalog, "images.request_budget_bytes").default == 37_748_736
    assert _by_path(catalog, "images.evict_quantum_bytes").default == 18_874_368


def test_declared_field_scale_matches_01_report() -> None:
    """规模断言：声明字段总数 = 68（01 报告的迁移字段数）；路径不重复。"""
    declared = _declared(build_catalog())
    assert len(declared) == 68
    paths = [node.path for node in declared]
    assert len(set(paths)) == len(paths)


# ─────────────────────────────────────────────────────────────────────────────
# 6. parse_path
# ─────────────────────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("a", [Key("a")]),
        ("_a1", [Key("_a1")]),
        ("a.b", [Key("a"), Key("b")]),
        ("a[0]", [Key("a"), Index(0)]),
        ("a[].b", [Key("a"), Element(), Key("b")]),
        ("a[0].b[1].c", [Key("a"), Index(0), Key("b"), Index(1), Key("c")]),
        (
            "providers[].models[].capabilities.vision",
            [
                Key("providers"),
                Element(),
                Key("models"),
                Element(),
                Key("capabilities"),
                Key("vision"),
            ],
        ),
        ("config.gateway.port", [Key("config"), Key("gateway"), Key("port")]),
        ("a[01]", [Key("a"), Index(1)]),
        (f"a[{2**64 - 1}]", [Key("a"), Index(2**64 - 1)]),
    ],
)
def test_parse_path_accepts_grammar(text: str, expected: list[object]) -> None:
    assert parse_path(text) == expected


@pytest.mark.parametrize(
    "text",
    [
        "",
        "a.",
        ".a",
        "a..b",
        "a[",
        "a]",
        "a[x]",
        "a[-1]",
        "a[0",
        "a[0]x",
        "a[]]",
        "1a",
        "a-b",
        "a b",
        "a[99999999999999999999]",  # > 2**64-1（溢出）
        "a[+1]",  # AD6：文法是「非负整数」，没有符号位（Rust 侧同此口径）
        "a[1_0]",
    ],
)
def test_parse_path_rejects_invalid(text: str) -> None:
    assert parse_path(text) is None


def test_parse_path_does_not_check_bounds() -> None:
    """下标不查界（增补 P3）：catalog 没有文档长度，越界下标照样解析成形态。"""
    assert parse_path("providers[7].api_key") == [
        Key("providers"),
        Index(7),
        Key("api_key"),
    ]
