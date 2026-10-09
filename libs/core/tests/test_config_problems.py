# tests/test_config_problems.py
"""跨字段检查单测 —— 每条检查的 path / kind / message / hint，以及加载期文案红线。

两个消费方共用同一批检查（``wing.config.problems``）：

- **加载期**：``Config._validate_config`` 只 raise ``problems[0].render()``，文案必须与
  抽取前**逐字一致**（``test_config.py`` / ``test_model_catalog.py`` /
  ``test_model_declaration.py`` / probe ``test_model_id_*`` 都钉着它）；
- **设置面板**：需要**全部**问题 + 精确路径，输入是 ``Config.model_construct(**raw)``
  造出的宽容视图（跨字段有问题时 ``Config(**raw)`` 根本构造不出来）。

``_raised(raw)`` 是红线断言：pydantic 加载路径实际 raise 的 message == 宽容视图上
``problems[0].render()``——两个消费方不许分叉。
"""

from __future__ import annotations

from dataclasses import FrozenInstanceError
from typing import Any

import pytest
from pydantic import ValidationError

from wing.config import AgentConfig, Config, ModelSpec, ProviderConfig, SettingMeta
from wing.config.problems import (
    UNKNOWN_ID_LIST_LIMIT,
    ConfigProblem,
    ModelRefView,
    ProblemKind,
    cross_field_problems,
    iter_model_refs,
    model_name_message,
    provider_model_problems,
    unknown_model_message,
)
from wing.config.spec import WING_META_KEY, setting_meta

# ─────────────────────────────────────────────────────────────────────────────
# 构造工具
# ─────────────────────────────────────────────────────────────────────────────


def _provider(**kwargs: Any) -> dict[str, Any]:
    fields: dict[str, Any] = {
        "name": "p",
        "base_url": "http://x",
        "api_key": "k",
        "models": ["m"],
    }
    fields.update(kwargs)
    return fields


def _raw(providers: list[Any], agents: list[Any], **extra: Any) -> dict[str, Any]:
    return {"providers": providers, "agents": agents, **extra}


def _view(raw: dict[str, Any]) -> Config:
    """宽容视图：不校验字段、不跑跨字段检查（面板侧的唯一可行形状）。"""
    return Config.model_construct(**raw)


def _problems(raw: dict[str, Any]) -> list[ConfigProblem]:
    return cross_field_problems(_view(raw))


def _raised(raw: dict[str, Any]) -> str:
    """加载路径实际 raise 的 message（剥掉 pydantic 的 ``Value error, `` 前缀）。"""
    with pytest.raises(ValidationError) as exc_info:
        Config(**raw)
    errors = exc_info.value.errors()
    assert len(errors) == 1, errors
    message = errors[0]["msg"]
    assert message.startswith("Value error, "), message
    return message.removeprefix("Value error, ")


def _typed(providers: list[ProviderConfig], agents: list[AgentConfig]) -> Config:
    """已构造对象的宽容视图（走属性读取，不走 dict 键）。"""
    return Config.model_construct(providers=providers, agents=agents)


# ─────────────────────────────────────────────────────────────────────────────
# 合法配置：没有问题（attribute 路径 + dict 路径）
# ─────────────────────────────────────────────────────────────────────────────


def test_valid_config_has_no_problems() -> None:
    config = Config(
        providers=[
            ProviderConfig(
                name="p",
                base_url="http://x",
                api_key="k",
                models=["m", ModelSpec(id="ds", name="dfmodel")],
            )
        ],
        agents=[AgentConfig(name="a", model="m"), AgentConfig(name="b", model="ds")],
    )
    assert cross_field_problems(config) == []
    assert cross_field_problems(config) == []  # 纯函数：可重复调用


def test_valid_typed_view_has_no_problems() -> None:
    """已构造对象走属性读取（不是 dict 键）——显式再钉一次。"""
    config = _typed(
        [ProviderConfig(name="p", base_url="http://x", api_key="k", models=["m"])],
        [AgentConfig(name="a", model="m")],
    )
    assert cross_field_problems(config) == []


# ─────────────────────────────────────────────────────────────────────────────
# 空列表（② / ③）
# ─────────────────────────────────────────────────────────────────────────────


def test_agents_empty() -> None:
    problems = _problems(_raw([_provider()], []))
    assert problems == [
        ConfigProblem(
            path="agents",
            kind=ProblemKind.EMPTY_LIST,
            message="agents list cannot be empty",
            hint="至少声明一个 agent，并让它引用某个 model id",
        )
    ]


def test_providers_empty() -> None:
    problems = _problems(_raw([], [{"name": "a", "model": "m"}]))
    assert problems == [
        ConfigProblem(
            path="providers",
            kind=ProblemKind.EMPTY_LIST,
            message="providers list cannot be empty",
            hint="至少声明一个 provider（模型目录的唯一来源）",
        )
    ]


def test_agents_reported_before_providers() -> None:
    """两个都空时报 agents（照抄今天的检查顺序，红线的一部分）。"""
    problems = _problems(_raw([], []))
    assert [p.path for p in problems] == ["agents", "providers"]
    assert _raised(_raw([], [])) == "agents list cannot be empty"


def test_empty_lists_and_no_models_use_attribute_path() -> None:
    """空列表走已构造对象：ProviderConfig 实例（models 为空）+ 空 agents。"""
    config = _typed([ProviderConfig(name="p", base_url="http://x", api_key="k")], [])
    assert [p.path for p in cross_field_problems(config)] == [
        "agents",
        "providers[0].models",
    ]
    assert [p.kind for p in cross_field_problems(config)] == [
        ProblemKind.EMPTY_LIST,
        ProblemKind.MISSING_REQUIRED,
    ]


# ─────────────────────────────────────────────────────────────────────────────
# 名字重复（④ / ⑤）
# ─────────────────────────────────────────────────────────────────────────────


def test_duplicate_provider_name_points_at_second_occurrence() -> None:
    problems = _problems(
        _raw(
            [
                _provider(name="x", models=["m1"]),
                _provider(name="y", models=["m2"]),
                _provider(name="x", models=["m3"]),
            ],
            [{"name": "a", "model": "m1"}],
        )
    )
    assert problems == [
        ConfigProblem(
            path="providers[2].name",
            kind=ProblemKind.DUPLICATE,
            message="duplicate provider name: 'x'",
            hint="重命名其中一个 provider",
        )
    ]


def test_duplicate_provider_name_each_name_once() -> None:
    problems = _problems(
        _raw(
            [
                _provider(name="x", models=["m1"]),
                _provider(name="y", models=["m2"]),
                _provider(name="x", models=["m3"]),
                _provider(name="y", models=["m4"]),
            ],
            [{"name": "a", "model": "m1"}],
        )
    )
    assert [p.path for p in problems] == ["providers[2].name", "providers[3].name"]
    assert [p.message for p in problems] == [
        "duplicate provider name: 'x'",
        "duplicate provider name: 'y'",
    ]


def test_duplicate_agent_name() -> None:
    problems = _problems(
        _raw([_provider()], [{"name": "a", "model": "m"}, {"name": "a", "model": "m"}])
    )
    assert problems == [
        ConfigProblem(
            path="agents[1].name",
            kind=ProblemKind.DUPLICATE,
            message="duplicate agent name: 'a'",
            hint="重命名其中一个 agent",
        )
    ]


# ─────────────────────────────────────────────────────────────────────────────
# ⑥ 模型目录：无模型 / id 全局唯一
# ─────────────────────────────────────────────────────────────────────────────


def test_provider_declares_no_models() -> None:
    problems = _problems(_raw([_provider(models=[])], [{"name": "a", "model": "m"}]))
    assert problems == [
        ConfigProblem(
            path="providers[0].models",
            kind=ProblemKind.MISSING_REQUIRED,
            message=(
                "provider 'p' declares no models: providers[].models must declare "
                "at least one model (the model catalog comes from configuration only)"
            ),
            hint="给该 provider 至少声明一个模型（裸字符串或对象形态）",
        )
    ]


def test_no_models_of_second_provider_is_reported_with_its_index() -> None:
    problems = _problems(
        _raw(
            [_provider(name="a", models=["m"]), _provider(name="b", models=[])],
            [{"name": "ag", "model": "m"}],
        )
    )
    assert [p.path for p in problems] == ["providers[1].models"]
    assert "provider 'b' declares no models" in problems[0].message


@pytest.mark.parametrize(
    "models,expected_path",
    [
        (["a", "a"], "providers[0].models[1]"),
        ([{"name": "a"}, {"name": "a"}], "providers[0].models[1].name"),
        (["a", {"name": "a"}], "providers[0].models[1].name"),
        ([{"name": "a"}, "a"], "providers[0].models[1]"),
    ],
)
def test_duplicate_model_name_reported_once_at_repeat(
    models: list[Any], expected_path: str
) -> None:
    """每个重复名字报一次，路径指到「第二次出现」那一项（对象形态指到 ``.name``）。

    同名的隐式 id 也撞车，所以「报全部」还会有第二条 id 冲突；加载期只 raise 第一条
    （调用名重复），顺序由 ``_RED_LINE_CASES`` 的 ``dup_model_name`` 钉住。
    """
    problems = _problems(
        _raw([_provider(models=models)], [{"name": "ag", "model": "a"}])
    )
    assert problems[0].kind == ProblemKind.DUPLICATE
    assert problems[0].message == "duplicate model name: 'a'"
    assert problems[0].path == expected_path
    assert [p.kind for p in problems[1:]] == [ProblemKind.DUPLICATE]
    assert "duplicate model id 'a'" in problems[1].message


def test_duplicate_model_name_object_form_path() -> None:
    problems = _problems(
        _raw(
            [_provider(models=[{"name": "a"}, {"name": "a"}])],
            [{"name": "ag", "model": "a"}],
        )
    )
    assert problems[0].path == "providers[0].models[1].name"


def test_provider_model_problems_without_prefix() -> None:
    """独立构造的 ProviderConfig 没有文档路径：路径相对该 provider。"""
    provider = ProviderConfig.model_construct(
        name="p", base_url="http://x", api_key="k", models=["a", "a"]
    )
    problems = provider_model_problems(provider)
    assert [p.path for p in problems] == ["models[1]"]
    assert problems[0].message == "duplicate model name: 'a'"


def test_duplicate_model_id_same_provider() -> None:
    problems = _problems(
        _raw(
            [_provider(name="a", models=[{"id": "shared", "name": "one"}, "shared"])],
            [{"name": "ag", "model": "shared"}],
        )
    )
    assert problems == [
        ConfigProblem(
            path="providers[0].models[1]",
            kind=ProblemKind.DUPLICATE,
            message=(
                "duplicate model id 'shared' declared twice by provider 'a'.\n"
                "Give one an explicit id, e.g.:\n"
                "  - id: a-shared\n"
                "    name: shared"
            ),
            hint="给其中一个声明显式 id（id 是全局唯一引用词）",
        )
    ]


def test_duplicate_model_id_cross_provider() -> None:
    problems = _problems(
        _raw(
            [
                _provider(name="a", models=[{"id": "gpt-4o", "name": "x"}]),
                _provider(name="b", models=["gpt-4o"]),
            ],
            [{"name": "ag", "model": "gpt-4o"}],
        )
    )
    assert problems[0].path == "providers[1].models[0]"
    assert problems[0].message == (
        "duplicate model id 'gpt-4o': declared by provider 'a' and provider 'b'.\n"
        "Give one an explicit id, e.g.:\n"
        "  - id: b-gpt-4o\n"
        "    name: gpt-4o"
    )


def test_duplicate_model_id_explicit_id_path() -> None:
    problems = _problems(
        _raw(
            [
                _provider(name="a", models=["same"]),
                _provider(name="b", models=[{"id": "same", "name": "other"}]),
            ],
            [{"name": "ag", "model": "same"}],
        )
    )
    assert problems[0].path == "providers[1].models[0].id"


# ─────────────────────────────────────────────────────────────────────────────
# ⑦ agents[].model 未命中 id 空间
# ─────────────────────────────────────────────────────────────────────────────


def test_unknown_agent_model() -> None:
    problems = _problems(
        _raw([_provider(models=["m"])], [{"name": "a", "model": "nope"}])
    )
    assert problems == [
        ConfigProblem(
            path="agents[0].model",
            kind=ProblemKind.UNKNOWN_REFERENCE,
            message="unknown model id 'nope'; available ids: m",
            hint="把 agents[].model 改成上面列出的某个 id",
        )
    ]


def test_unknown_agent_model_call_name_hint() -> None:
    problems = _problems(
        _raw(
            [_provider(name="local", models=[{"id": "ds-flash", "name": "sonnet"}])],
            [{"name": "a", "model": "sonnet"}],
        )
    )
    assert problems[0].message == (
        "unknown model id 'sonnet'; available ids: ds-flash; "
        "note: 'sonnet' is the call name of model id 'ds-flash' (provider 'local') — "
        "declare an explicit id or send 'ds-flash'"
    )


def test_unknown_agent_model_multi_call_name_hint() -> None:
    problems = _problems(
        _raw(
            [
                _provider(name="p1", models=[{"id": "id-one", "name": "shared"}]),
                _provider(name="p2", models=[{"id": "id-two", "name": "shared"}]),
            ],
            [{"name": "a", "model": "shared"}],
        )
    )
    assert problems[0].message == (
        "unknown model id 'shared'; available ids: id-one, id-two; "
        "note: 'shared' is the call name of model ids 'id-one' (provider 'p1') and "
        "'id-two' (provider 'p2') — declare an explicit id or send one of those ids"
    )


def test_unknown_agent_model_truncates_available_ids() -> None:
    providers = [
        _provider(name=f"p{i:02d}", models=[f"model-{i:02d}"]) for i in range(12)
    ]
    problems = _problems(_raw(providers, [{"name": "a", "model": "ghost"}]))
    shown = ", ".join(f"model-{i:02d}" for i in range(UNKNOWN_ID_LIST_LIMIT))
    assert problems[0].message == f"unknown model id 'ghost'; available ids: {shown}, …"


def test_unknown_agent_model_is_trimmed() -> None:
    """查找与 ``find_model()`` 同一 trim 语义；请求值原样进文案。"""
    assert (
        _problems(_raw([_provider(models=["m"])], [{"name": "a", "model": " m "}]))
        == []
    )
    problems = _problems(
        _raw([_provider(models=["m"])], [{"name": "a", "model": " ghost "}])
    )
    assert problems[0].message == "unknown model id ' ghost '; available ids: m"


def test_invalid_call_name_declaration_is_not_part_of_catalog() -> None:
    """调用名非法的声明不算目录的一部分（不污染 available ids），由字段校验报。"""
    raw = _raw([_provider(models=[" m "])], [{"name": "a", "model": " m "}])
    assert _problems(raw) == []
    # 加载路径报的是调用名守门（经 model_names()），不是「未命中」
    assert (
        _raised(raw)
        == "model name must not have leading/trailing whitespace, got: ' m '"
    )


def test_each_unknown_agent_reported() -> None:
    problems = _problems(
        _raw(
            [_provider(models=["m"])],
            [
                {"name": "a", "model": "nope"},
                {"name": "b", "model": "m"},
                {"name": "c", "model": "nada"},
            ],
        )
    )
    assert [p.path for p in problems] == ["agents[0].model", "agents[2].model"]


# ─────────────────────────────────────────────────────────────────────────────
# 顺序约定：① provider 内重复调用名 → ② agents 空 → ③ providers 空 → ④…⑦
# ─────────────────────────────────────────────────────────────────────────────


def test_duplicate_model_name_preempts_agents_empty() -> None:
    """嵌套模型校验先于 Config 级检查（pydantic 路径的顺序，必须一致）。"""
    raw = _raw([_provider(models=["a", "a"])], [])
    assert _problems(raw)[0].message == "duplicate model name: 'a'"
    assert _raised(raw) == "duplicate model name: 'a'"


def test_provider_order_preempts_later_provider_checked_first() -> None:
    """provider[0] 的 id 冲突先于 provider[1] 的「无模型」（照抄今天的循环结构）。"""
    raw = _raw(
        [
            _provider(name="a", models=["x"]),
            _provider(name="b", models=["x"]),
            _provider(name="c", models=[]),
        ],
        [{"name": "ag", "model": "x"}],
    )
    assert _problems(raw)[0].path == "providers[1].models[0]"
    raw2 = _raw(
        [_provider(name="a", models=["x"]), _provider(name="b", models=[])],
        [{"name": "ag", "model": "x"}],
    )
    assert _problems(raw2)[0].path == "providers[1].models"


# ─────────────────────────────────────────────────────────────────────────────
# 红线：加载期 message == problems[0].render()（逐字）
# ─────────────────────────────────────────────────────────────────────────────

_RED_LINE_CASES: list[tuple[str, dict[str, Any]]] = [
    ("providers_empty", _raw([], [{"name": "a", "model": "m"}])),
    ("agents_empty", _raw([_provider()], [])),
    ("both_empty", _raw([], [])),
    (
        "dup_provider_name",
        _raw(
            [_provider(name="x", models=["m1"]), _provider(name="x", models=["m2"])],
            [{"name": "a", "model": "m1"}],
        ),
    ),
    (
        "dup_agent_name",
        _raw([_provider()], [{"name": "a", "model": "m"}, {"name": "a", "model": "m"}]),
    ),
    ("provider_no_models", _raw([_provider(models=[])], [{"name": "a", "model": "m"}])),
    (
        "dup_model_id_same_provider",
        _raw(
            [_provider(name="a", models=[{"id": "shared", "name": "one"}, "shared"])],
            [{"name": "ag", "model": "shared"}],
        ),
    ),
    (
        "dup_model_id_cross_provider",
        _raw(
            [
                _provider(name="a", models=[{"id": "gpt-4o", "name": "x"}]),
                _provider(name="b", models=["gpt-4o"]),
            ],
            [{"name": "ag", "model": "gpt-4o"}],
        ),
    ),
    (
        "dup_model_name",
        _raw([_provider(models=["a", "a"])], [{"name": "ag", "model": "a"}]),
    ),
    (
        "unknown_agent_model",
        _raw([_provider(models=["m"])], [{"name": "a", "model": "nope"}]),
    ),
    (
        "unknown_agent_model_call_name",
        _raw(
            [_provider(name="local", models=[{"id": "ds-flash", "name": "sonnet"}])],
            [{"name": "a", "model": "sonnet"}],
        ),
    ),
]


@pytest.mark.parametrize(
    "raw",
    [raw for _, raw in _RED_LINE_CASES],
    ids=[name for name, _ in _RED_LINE_CASES],
)
def test_load_path_message_equals_first_problem(raw: dict[str, Any]) -> None:
    """加载期 raise 的 message 与 ``problems[0].render()`` 逐字相同（R1 机制）。"""
    assert _raised(raw) == _problems(raw)[0].render()


# ─────────────────────────────────────────────────────────────────────────────
# 宽容视图：脏输入不抛异常、不误报
# ─────────────────────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "raw",
    [
        {"providers": "nope", "agents": []},
        {"providers": None, "agents": None},
        {"providers": [None, 1, "x"], "agents": [None]},
        {"providers": [{"models": "m"}], "agents": [{"model": 7}]},
        {"providers": [{"name": 1, "models": ["m"]}], "agents": []},
        {
            "providers": [{"name": "p", "models": [None, 3]}],
            "agents": [{"name": "a", "model": "m"}],
        },
        {},
    ],
)
def test_dirty_view_never_raises(raw: dict[str, Any]) -> None:
    """字段级校验的地盘（形态无法判定）一律跳过：不抛、不猜。"""
    problems = _problems(raw)
    assert all(isinstance(p, ConfigProblem) for p in problems)


def test_lenient_view_matches_typed_view() -> None:
    """同一份数据：宽容视图与「已构造对象」产出同一批问题。"""
    from_view = _problems(
        _raw(
            [_provider(name="a", models=["x"]), _provider(name="b", models=[])],
            [{"name": "ag", "model": "nope"}],
        )
    )
    typed = _typed(
        [
            ProviderConfig(name="a", base_url="http://x", api_key="k", models=["x"]),
            ProviderConfig(name="b", base_url="http://x", api_key="k", models=[]),
        ],
        [AgentConfig(name="ag", model="nope")],
    )
    assert cross_field_problems(typed) == from_view
    assert [p.path for p in from_view] == ["providers[1].models", "agents[0].model"]


def test_iter_model_refs_skips_unknown_shapes() -> None:
    providers = [
        {"name": "p", "models": ["a", None, {"name": "b"}, {"id": 3, "name": "c"}]}
    ]
    refs = list(iter_model_refs(providers))
    assert [(ref.id, ref.name, ref.provider_name) for ref in refs] == [
        ("a", "a", "p"),
        ("b", "b", "p"),
    ]


# ─────────────────────────────────────────────────────────────────────────────
# 调用名守门文案（唯一实现，由 ModelSpec 字段校验器与检查共用）
# ─────────────────────────────────────────────────────────────────────────────


def test_model_name_message() -> None:
    assert model_name_message("m") is None
    assert model_name_message("a b") is None  # 内部空格合法（只有首尾被禁）
    assert model_name_message(7) is None  # 无法判定：交给字段类型检查
    assert model_name_message("") == "model name must be non-empty"
    assert model_name_message("   ") == "model name must be non-empty"
    assert model_name_message(" m ") == (
        "model name must not have leading/trailing whitespace, got: ' m '"
    )


# ─────────────────────────────────────────────────────────────────────────────
# unknown_model_message（唯一实现；Config.describe_unknown_model 委托它）
# ─────────────────────────────────────────────────────────────────────────────


def _refs(*spec: tuple[str, str, str]) -> list[ModelRefView]:
    return [ModelRefView(*item) for item in spec]


def test_unknown_model_message_basic() -> None:
    refs = _refs(("a", "a", "p1"), ("b", "b", "p2"))
    assert unknown_model_message(refs, "nope") == (
        "unknown model id 'nope'; available ids: a, b"
    )


def test_unknown_model_message_no_ids() -> None:
    assert (
        unknown_model_message([], "nope") == "unknown model id 'nope'; available ids: "
    )


def test_unknown_model_message_call_name_note() -> None:
    refs = _refs(("ds-flash", "sonnet", "local"))
    assert unknown_model_message(refs, "sonnet") == (
        "unknown model id 'sonnet'; available ids: ds-flash; "
        "note: 'sonnet' is the call name of model id 'ds-flash' (provider 'local') — "
        "declare an explicit id or send 'ds-flash'"
    )


def test_unknown_model_message_preserves_requested_value() -> None:
    """请求值原样进文案（未 trim），查找提示用 trim 后的值。"""
    refs = _refs(("m", "m", "p"))
    assert unknown_model_message(refs, " m ") == (
        "unknown model id ' m '; available ids: m; "
        "note: ' m ' is the call name of model id 'm' (provider 'p') — "
        "declare an explicit id or send 'm'"
    )
    assert unknown_model_message(refs, " ghost ") == (
        "unknown model id ' ghost '; available ids: m"
    )


def test_describe_unknown_model_delegates() -> None:
    """``Config.describe_unknown_model``（运行期出口）与纯函数输出一致。"""
    config = Config(
        providers=[
            ProviderConfig(
                name="local",
                base_url="http://x",
                api_key="k",
                models=[ModelSpec(id="ds-flash", name="sonnet")],
            )
        ],
        agents=[AgentConfig(name="a", model="ds-flash")],
    )
    refs = list(iter_model_refs(config.providers))
    for model_id in ["nope", "sonnet", " ghost ", "ds-flash"]:
        assert config.describe_unknown_model(model_id) == unknown_model_message(
            refs, model_id
        )


# ─────────────────────────────────────────────────────────────────────────────
# ConfigProblem / ProblemKind 自身
# ─────────────────────────────────────────────────────────────────────────────


def test_render_is_message_and_excludes_path_and_hint() -> None:
    problem = ConfigProblem(
        path="agents[0].model",
        kind=ProblemKind.UNKNOWN_REFERENCE,
        message="boom",
        hint="fix it",
    )
    assert problem.render() == "boom"
    assert "fix it" not in problem.render()
    assert "agents[0].model" not in problem.render()


def test_problem_is_frozen_and_kinds_are_wire_strings() -> None:
    problem = ConfigProblem(path=None, kind=ProblemKind.CONFLICT, message="x")
    with pytest.raises(FrozenInstanceError):
        setattr(problem, "message", "y")
    assert [kind.value for kind in ProblemKind] == [
        "missing_required",
        "invalid_value",
        "empty_list",
        "duplicate",
        "unknown_reference",
        "conflict",
        "unknown_key",
    ]
    assert ProblemKind.DUPLICATE == "duplicate"


# ─────────────────────────────────────────────────────────────────────────────
# 声明层与检查的一致性（同一事实的两处表达，防漂移）
# ─────────────────────────────────────────────────────────────────────────────


def test_min_items_declaration_matches_empty_checks() -> None:
    """声明了 min_items=1 的字段恰是产出空列表 / 无模型 problem 的那些字段。"""
    declared = sorted(
        f"{model.__name__}.{name}"
        for model in (Config, ProviderConfig)
        for name, field in model.model_fields.items()
        if (meta := setting_meta(field)) is not None and meta.min_items is not None
    )
    assert declared == ["Config.agents", "Config.providers", "ProviderConfig.models"]

    empty_kinds = {
        p.kind for p in _problems(_raw([], [])) if p.kind == ProblemKind.EMPTY_LIST
    }
    assert empty_kinds == {ProblemKind.EMPTY_LIST}
    assert [
        p.kind
        for p in _problems(_raw([_provider(models=[])], [{"name": "a", "model": "m"}]))
    ] == [ProblemKind.MISSING_REQUIRED]


def test_setting_meta_is_the_read_accessor() -> None:
    field = Config.model_fields["providers"]
    meta = setting_meta(field)
    assert isinstance(meta, SettingMeta)
    assert meta.min_items == 1
    extra = field.json_schema_extra
    assert isinstance(extra, dict) and WING_META_KEY in extra
