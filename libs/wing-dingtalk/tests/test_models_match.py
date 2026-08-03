"""models_match 单测。"""

from wing_dingtalk.models_match import flatten, match_model

GROUPS = [
    {"provider": "proxy", "models": ["qwen3.8-max-preview", "deepseek-v4-flash-0731"]},
    {"provider": "dashscope", "models": ["qwen-turbo", "deepseek-v4-flash-0731"]},
]


def test_flatten() -> None:
    refs = flatten(GROUPS)
    assert len(refs) == 4
    assert str(refs[0]) == "proxy:qwen3.8-max-preview"


def test_exact_match_across_providers() -> None:
    result = match_model(GROUPS, "qwen-turbo")
    assert result.ok
    assert result.hit is not None
    assert result.hit.provider == "dashscope"
    assert result.hit.model == "qwen-turbo"


def test_exact_match_case_insensitive() -> None:
    result = match_model(GROUPS, "QWEN-TURBO")
    assert result.ok
    assert result.hit is not None
    assert result.hit.model == "qwen-turbo"


def test_substring_unique() -> None:
    result = match_model(GROUPS, "max-preview")
    assert result.ok
    assert result.hit is not None
    assert result.hit.model == "qwen3.8-max-preview"


def test_ambiguous_across_providers() -> None:
    # deepseek-v4-flash-0731 在两个 provider 都有
    result = match_model(GROUPS, "deepseek")
    assert not result.ok
    assert len(result.candidates) == 2


def test_provider_prefix_disambiguates() -> None:
    result = match_model(GROUPS, "dashscope:deepseek")
    assert result.ok
    assert result.hit is not None
    assert result.hit.provider == "dashscope"


def test_provider_prefix_slash_form() -> None:
    result = match_model(GROUPS, "proxy/deepseek")
    assert result.ok
    assert result.hit is not None
    assert result.hit.provider == "proxy"


def test_provider_prefix_case_insensitive() -> None:
    result = match_model(GROUPS, "PROXY:qwen3.8")
    assert result.ok
    assert result.hit is not None
    assert result.hit.provider == "proxy"


def test_unknown_provider_prefix_falls_back_to_global() -> None:
    # 首段不是已知 provider → 整体作为模型名匹配
    result = match_model(GROUPS, "nosuch:qwen-turbo")
    assert not result.ok  # "nosuch:qwen-turbo" 不是任何模型的子串


def test_no_match() -> None:
    result = match_model(GROUPS, "gpt-9")
    assert not result.ok
    assert result.error is not None
    assert "no model matching" in result.error


def test_empty_groups() -> None:
    result = match_model([], "anything")
    assert not result.ok
    assert result.error is not None
    assert "no models" in result.error


def test_empty_query() -> None:
    result = match_model(GROUPS, "  ")
    assert not result.ok
