# tests/test_config_spec.py
"""声明层门禁 —— 每个配置项必须经 ``S(...)`` 声明，且声明形状合法。

**这是机制，不是纪律**：新增字段忘了声明、`apply` 忘了显式写、字段 docstring 与
`doc`/`notes` 两边都留（新的 SYNC），都会在这里变红。

规则清单（对应 ``libs/core/wing/config/spec.py``）：

1. 递归遍历 ``Config`` 可达的全部 ``BaseModel``，每个字段必须有 ``json_schema_extra["wing"]``；
   且 ``models.py`` 里不许有不可达的孤儿模型（漏扫 = 门禁失效）。
2. ``doc`` 非空且首行 ≤ 60 字符（面板行内提示要放得下）；``notes`` 若给必须非空。
3. AST：源码里每个 ``S(...)`` 调用都**显式**带 ``apply=``（默认值只是为了让模型可构造）。
4. ``secret=True`` 的字段注解必须是 ``str``。
5. ``choices`` 的键集 ⊇ ``Literal`` 的 args（有 ``Literal`` 时）。
6. AST：字段赋值后不紧跟字符串表达式（无残留字段 docstring）。
7. 顶层九分组：名字闭集、声明序连续、``section_doc`` 每段恰好一次且在首字段。
8. 容器节点的 ``apply`` = 子树**最粗**的一档（总设计只给了叶子，容器约定见 design.md D7）。
9. 任务书点名的声明（secret / min_items / summary_fields / choices）逐条钉住。
"""

from __future__ import annotations

import ast
from pathlib import Path
from typing import Any, Literal, get_args, get_origin

import pytest
from pydantic import BaseModel
from pydantic.fields import FieldInfo

from wing.config import models as models_module
from wing.config.models import Config
from wing.config.spec import ApplyScope, setting_meta

# ─────────────────────────────────────────────────────────────────────────────
# 模型遍历
# ─────────────────────────────────────────────────────────────────────────────


def _declared_models() -> list[type[BaseModel]]:
    """``models.py`` 里定义的全部 pydantic 模型（按源码顺序）。"""
    return [
        obj
        for obj in vars(models_module).values()
        if isinstance(obj, type)
        and issubclass(obj, BaseModel)
        and obj.__module__ == models_module.__name__
    ]


def _child_models(annotation: Any) -> list[type[BaseModel]]:
    """注解里嵌的 ``BaseModel`` 子类（递归剥 ``list`` / ``union`` / ``None``）。"""
    if isinstance(annotation, type) and issubclass(annotation, BaseModel):
        return [annotation]
    out: list[type[BaseModel]] = []
    for arg in get_args(annotation):
        out.extend(_child_models(arg))
    return out


def _walk_models(
    root: type[BaseModel],
) -> dict[type[BaseModel], list[tuple[str, FieldInfo]]]:
    """``root`` 可达的全部模型 → 字段列表（保持 ``model_fields`` 顺序）。"""
    walked: dict[type[BaseModel], list[tuple[str, FieldInfo]]] = {}
    stack: list[type[BaseModel]] = [root]
    while stack:
        model = stack.pop()
        if model in walked:
            continue
        fields = list(model.model_fields.items())
        walked[model] = fields
        for _, field in fields:
            stack.extend(
                child
                for child in _child_models(field.annotation)
                if child not in walked
            )
    return walked


_REACHABLE = _walk_models(Config)
_ALL_FIELDS: list[tuple[str, FieldInfo]] = [
    (f"{model.__name__}.{name}", field)
    for model, fields in _REACHABLE.items()
    for name, field in fields
]
_FIELD_IDS = [path for path, _ in _ALL_FIELDS]

_MODELS_SOURCE_PATH = Path(models_module.__file__)
_MODELS_SOURCE = _MODELS_SOURCE_PATH.read_text(encoding="utf-8")
_MODELS_TREE = ast.parse(_MODELS_SOURCE, filename=str(_MODELS_SOURCE_PATH))


_FIELD_BY_PATH: dict[str, FieldInfo] = dict(_ALL_FIELDS)


def _meta(path: str) -> Any:
    meta = setting_meta(_FIELD_BY_PATH[path])
    assert meta is not None, f"{path} 没有声明（缺 S(...)）"
    return meta


def _meta_for(field: FieldInfo) -> Any:
    meta = setting_meta(field)
    assert meta is not None, "字段没有声明"
    return meta


def _literal_args(annotation: Any) -> list[str]:
    """注解里 ``Literal[...]`` 的字符串 args（递归剥 union / ``None``）。"""
    if get_origin(annotation) is Literal:
        return [arg for arg in get_args(annotation) if isinstance(arg, str)]
    out: list[str] = []
    for arg in get_args(annotation):
        out.extend(_literal_args(arg))
    return out


# ─────────────────────────────────────────────────────────────────────────────
# 1. 每个字段都有声明
# ─────────────────────────────────────────────────────────────────────────────


def test_every_declared_model_is_reachable() -> None:
    """``models.py`` 里的模型必须全部从 ``Config`` 可达（否则下面的门禁漏扫）。"""
    unreachable = sorted(
        model.__name__ for model in _declared_models() if model not in _REACHABLE
    )
    assert not unreachable, f"以下模型从 Config 不可达，声明门禁扫不到：{unreachable}"


def test_field_count_is_sane() -> None:
    """健全性：字段规模与声明层现状一致（防止遍历器写坏导致假绿）。"""
    assert len(_ALL_FIELDS) == 68, [path for path, _ in _ALL_FIELDS]


@pytest.mark.parametrize("path", _FIELD_IDS)
def test_every_field_has_declaration(path: str) -> None:
    """每个字段都必须有 ``S(...)`` 声明（元信息挂在 ``json_schema_extra["wing"]``）。"""
    assert setting_meta(_FIELD_BY_PATH[path]) is not None


# ─────────────────────────────────────────────────────────────────────────────
# 2. doc / notes 形状
# ─────────────────────────────────────────────────────────────────────────────


@pytest.mark.parametrize("path", _FIELD_IDS)
def test_doc_is_present_and_short(path: str) -> None:
    meta = _meta(path)
    assert meta.doc.strip(), f"{path}: doc 为空"
    first_line = meta.doc.splitlines()[0]
    assert len(first_line) <= 60, f"{path}: doc 首行 {len(first_line)} 字符（> 60）"


@pytest.mark.parametrize("path", _FIELD_IDS)
def test_notes_are_non_empty_when_given(path: str) -> None:
    meta = _meta(path)
    if meta.notes is not None:
        assert meta.notes.strip(), f"{path}: notes 为空串（要么别给，要么给内容）"


# ─────────────────────────────────────────────────────────────────────────────
# 3 / 6. 源码级门禁（AST）
# ─────────────────────────────────────────────────────────────────────────────


def _s_calls() -> list[ast.Call]:
    return [
        node
        for node in ast.walk(_MODELS_TREE)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "S"
    ]


def test_apply_is_explicit_in_every_declaration() -> None:
    """每个 ``S(...)`` 调用都必须显式写 ``apply=``（默认值不算声明）。"""
    calls = _s_calls()
    assert len(calls) == len(_ALL_FIELDS), (
        f"源码里 {len(calls)} 个 S(...) 调用，字段却有 {len(_ALL_FIELDS)} 个——门禁失效"
    )
    missing = sorted(
        node.lineno
        for node in calls
        if not any(kw.arg == "apply" for kw in node.keywords)
    )
    assert not missing, f"以下 S(...) 调用没写 apply=：lines {missing}"


def test_no_field_docstrings_remain() -> None:
    """字段 docstring 必须已整段搬进 ``doc``/``notes``（留着 = 新的 SYNC）。"""
    violations: list[str] = []
    for model in _declared_models():
        class_node = next(
            (
                node
                for node in _MODELS_TREE.body
                if isinstance(node, ast.ClassDef) and node.name == model.__name__
            ),
            None,
        )
        assert class_node is not None, f"源码里找不到 class {model.__name__}"
        body = class_node.body
        for index, stmt in enumerate(body[:-1]):
            if not (
                isinstance(stmt, ast.AnnAssign)
                and isinstance(stmt.target, ast.Name)
                and stmt.target.id in model.model_fields
            ):
                continue
            nxt = body[index + 1]
            if (
                isinstance(nxt, ast.Expr)
                and isinstance(nxt.value, ast.Constant)
                and isinstance(nxt.value.value, str)
            ):
                violations.append(f"{model.__name__}.{stmt.target.id}")
    assert not violations, f"以下字段还留着 docstring：{violations}"


# ─────────────────────────────────────────────────────────────────────────────
# 4 / 5. 类型与枚举
# ─────────────────────────────────────────────────────────────────────────────


@pytest.mark.parametrize("path", _FIELD_IDS)
def test_secret_fields_are_str(path: str) -> None:
    field = _FIELD_BY_PATH[path]
    if _meta(path).secret:
        assert field.annotation is str, f"{path}: secret 字段必须是 str"


@pytest.mark.parametrize("path", _FIELD_IDS)
def test_choices_cover_literal_args(path: str) -> None:
    field = _FIELD_BY_PATH[path]
    literals = _literal_args(field.annotation)
    if not literals:
        return
    choices = _meta(path).choices or {}
    missing = sorted(set(literals) - set(choices))
    assert not missing, f"{path}: choices 缺 {missing}（Literal 的值域必须都有含义）"


# ─────────────────────────────────────────────────────────────────────────────
# 7. 顶层九分组
# ─────────────────────────────────────────────────────────────────────────────

_EXPECTED_SECTIONS = [
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


def test_config_sections_are_contiguous_and_documented() -> None:
    """九分组：名字闭集、声明序连续、``section_doc`` 每段恰好一次且在首字段。"""
    sections: list[str] = []
    for name in Config.model_fields:
        meta = _meta(f"Config.{name}")
        assert meta.section is not None, f"Config.{name} 缺 section"
        sections.append(meta.section)

    # 连续 = 同一 section 只出现在一段里（分组顺序 = 声明序）。
    first_seen: list[str] = []
    for section in sections:
        if section not in first_seen:
            first_seen.append(section)
    assert first_seen == _EXPECTED_SECTIONS, f"分组顺序不符：{first_seen}"

    for section in _EXPECTED_SECTIONS:
        section_fields = [
            field
            for name, field in Config.model_fields.items()
            if _meta(f"Config.{name}").section == section
        ]
        docs = [_meta_for(field).section_doc for field in section_fields]
        assert sum(doc is not None for doc in docs) == 1, (
            f"{section}: section_doc 必须恰好写一次（写多处 = 新 SYNC）"
        )
        assert docs[0] is not None, f"{section}: section_doc 必须写在首字段上"


def test_nested_models_have_no_section() -> None:
    """``section`` 只属于顶层字段（嵌套模型的字段不该有分组）。"""
    for path, _ in _ALL_FIELDS:
        model_name = path.split(".")[0]
        if model_name == "Config":
            continue
        assert _meta(path).section is None, f"{path}: 嵌套字段不该有 section"


# ─────────────────────────────────────────────────────────────────────────────
# 8. 容器 apply = 子树最粗
# ─────────────────────────────────────────────────────────────────────────────

_APPLY_RANK = {
    ApplyScope.HOT: 0,
    ApplyScope.NEXT_SESSION: 1,
    ApplyScope.RESTART: 2,
    ApplyScope.READONLY: 3,
}


def _subtree_apply(model: type[BaseModel]) -> ApplyScope:
    """一个模型子树的**最粗**生效域（递归；容器取子树最大值）。"""
    worst = ApplyScope.HOT
    for field in model.model_fields.values():
        meta = setting_meta(field)
        assert meta is not None, f"{model.__name__} 有字段没有声明"
        children = _child_models(field.annotation)
        current = (
            min(
                (_subtree_apply(child) for child in children),
                key=lambda scope: _APPLY_RANK[scope],
            )
            if children
            else meta.apply
        )
        if _APPLY_RANK[current] > _APPLY_RANK[worst]:
            worst = current
    return worst


@pytest.mark.parametrize("path", _FIELD_IDS)
def test_container_apply_is_coarsest_of_subtree(path: str) -> None:
    """容器节点的 ``apply`` = 子树最粗的一档（design.md D7）。"""
    field = _FIELD_BY_PATH[path]
    children = _child_models(field.annotation)
    if not children:
        return
    expected = min(
        (_subtree_apply(child) for child in children),
        key=lambda scope: _APPLY_RANK[scope],
    )
    assert _meta(path).apply == expected, f"{path}: 容器 apply 应为 {expected}"


def test_required_fields_are_declared_without_default() -> None:
    """必填 = 「没有 default」（pydantic 的 ``is_required()``），不引入第二个 ``required=``。

    注：``S()`` 对 ty 不透明，类型检查器不再推断这些模型的必填性——这条测试是那层
    保护的机制替代（集合变化必须显式改测试）。
    """
    required = sorted(
        path for path, field in _ALL_FIELDS if _FIELD_BY_PATH[path].is_required()
    )
    assert required == [
        "AgentConfig.model",
        "AgentConfig.name",
        "ApiKeyEntry.key",
        "Config.agents",
        "Config.providers",
        "ModelSpec.name",
        "ProviderConfig.api_key",
        "ProviderConfig.base_url",
        "ProviderConfig.name",
    ]
    assert _meta("Config.providers").min_items == 1  # 必填 + min_items=1 成对


# ─────────────────────────────────────────────────────────────────────────────
# 9. 任务书点名的声明（防漂移）
# ─────────────────────────────────────────────────────────────────────────────


def test_secret_declarations_match_spec() -> None:
    secrets = sorted(path for path, _ in _ALL_FIELDS if _meta(path).secret)
    assert secrets == ["ApiKeyEntry.key", "ProviderConfig.api_key"]


def test_min_items_declarations_match_spec() -> None:
    with_min_items = sorted(
        path for path, _ in _ALL_FIELDS if _meta(path).min_items is not None
    )
    assert with_min_items == [
        "Config.agents",
        "Config.providers",
        "ProviderConfig.models",
    ]
    assert all(_meta(path).min_items == 1 for path in with_min_items)


def test_summary_fields_declarations_match_spec() -> None:
    summary = {path: _meta(path).summary_fields for path, _ in _ALL_FIELDS}
    declared = {path: value for path, value in summary.items() if value is not None}
    assert declared == {
        "Config.agents": ["name", "model"],
        "Config.providers": ["name", "protocol", "base_url"],
        "ProviderConfig.models": ["id", "name", "display_name"],
    }


def test_choices_declarations_match_spec() -> None:
    declared = {
        path: set(_meta(path).choices or {})
        for path, _ in _ALL_FIELDS
        if _meta(path).choices is not None
    }
    assert declared == {
        "ApiKeyEntry.role": {"admin", "tool_runtime"},
        "LogConfig.level": {"DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"},
        "ProviderConfig.image_delivery": {"inline", "followup"},
        "ProviderConfig.protocol": {"openai", "anthropic"},
        # 值域原先只写在 notes 里的 str 字段（增补 P10）：补 choices ⇒ catalog 推成 enum
        "ProviderConfig.reasoning_effort": {"low", "medium", "high", "max"},
        "UserAgentConfig.preset": {"opencode", "qwen-code"},
    }
