"""provider 连接池上限（``providers[].max_connections``）的装配契约。

被守的语义：

- **缺省零变化**：``max_connections`` 未声明时 ``httpx.AsyncClient(...)`` 收到的 kwargs
  与引入该旋钮之前逐字相同——**根本没有** ``limits`` 这个键。显式传一个"近似默认"的
  ``Limits()`` 也算不合格：它把 httpx 的默认连接池口径抄进了本仓库（httpx 改默认即失真）；
- 声明值 → ``Limits(max_connections=n, max_keepalive_connections=min(n, 20))``；
- 非法值（``0`` / 负数）落在**配置校验期**并带精确路径，而不是等到构造 provider 才炸栈；
- 两个协议实现同形（openai / anthropic 各一个 ``AsyncClient``）。

证据方式是**捕获构造 kwargs**：``provider._client`` 是 httpx 内部状态的间接投影，
而「传没传 ``limits``」只有构造那一刻是事实。
"""

from __future__ import annotations

from typing import Any

import httpx
import pytest

from wing.config import ProviderConfig
from wing.config.document import locate_problems
from wing.provider.anthropic.provider import AnthropicProvider
from wing.provider.openai.provider import OpenAICompatProvider
from wing.provider.transport import make_http_limits

PROTOCOLS = ("openai", "anthropic")

#: provider 构造只碰这三个 httpx 关键字——多一个就是行为变化。
BASE_CLIENT_KWARGS = {"base_url", "headers", "timeout"}


@pytest.fixture
def client_kwargs(monkeypatch: pytest.MonkeyPatch) -> list[dict[str, Any]]:
    """截获 ``httpx.AsyncClient`` 的构造 kwargs（两个 provider 模块共用同一份 httpx）。"""
    captured: list[dict[str, Any]] = []

    class _CapturingClient:
        """替身：provider 构造期不碰 IO，只记录参数。"""

        def __init__(self, **kwargs: Any) -> None:
            captured.append(kwargs)

    monkeypatch.setattr(httpx, "AsyncClient", _CapturingClient)
    return captured


def make_provider(protocol: str, **overrides: Any) -> None:
    """按协议构造一个 provider（配置走 conftest 的 Config 单例，headers 可解析）。"""
    cfg = ProviderConfig(
        name=f"limit-{protocol}",
        protocol=protocol,  # ty: ignore[invalid-argument-type]
        base_url="https://api.example.com/v1",
        api_key="sk-test",
        models=["m"],
        **overrides,
    )
    if protocol == "openai":
        OpenAICompatProvider(cfg)
    else:
        AnthropicProvider(cfg)


# ── 1. 缺省零变化 ──────────────────────────────────────────────


@pytest.mark.parametrize("protocol", PROTOCOLS)
def test_default_sends_no_limits_kwarg(
    protocol: str, client_kwargs: list[dict[str, Any]]
) -> None:
    """未配置时不给 ``AsyncClient`` 传 ``limits``（与改动前逐字相同）。"""
    make_provider(protocol)

    assert len(client_kwargs) == 1
    kwargs = client_kwargs[0]
    assert "limits" not in kwargs, kwargs
    assert set(kwargs) == BASE_CLIENT_KWARGS, sorted(kwargs)


@pytest.mark.parametrize("protocol", PROTOCOLS)
def test_explicit_none_is_the_same_as_absent(
    protocol: str, client_kwargs: list[dict[str, Any]]
) -> None:
    """显式写 ``max_connections: null`` 与不写等价（YAML 两种形态都常见）。"""
    make_provider(protocol, max_connections=None)

    assert set(client_kwargs[0]) == BASE_CLIENT_KWARGS, sorted(client_kwargs[0])


# ── 2. 声明值 → httpx.Limits ───────────────────────────────────


@pytest.mark.parametrize("protocol", PROTOCOLS)
def test_declared_limit_reaches_async_client(
    protocol: str, client_kwargs: list[dict[str, Any]]
) -> None:
    """配置 7 → 连接上限 7、keepalive 上限 7。"""
    make_provider(protocol, max_connections=7)

    assert client_kwargs[0]["limits"] == httpx.Limits(
        max_connections=7, max_keepalive_connections=7
    )


@pytest.mark.parametrize("protocol", PROTOCOLS)
def test_keepalive_is_capped_at_twenty(
    protocol: str, client_kwargs: list[dict[str, Any]]
) -> None:
    """调大连接上限不抬高空闲 socket 驻留量：keepalive 钳在 20。"""
    make_provider(protocol, max_connections=64)

    assert client_kwargs[0]["limits"] == httpx.Limits(
        max_connections=64, max_keepalive_connections=20
    )


def test_make_http_limits_returns_none_when_unset() -> None:
    """``None`` 是「不设限」的唯一表达（调用方据此决定不传该键）。"""
    assert make_http_limits(None) is None
    assert make_http_limits(1) == httpx.Limits(
        max_connections=1, max_keepalive_connections=1
    )
    assert make_http_limits(100) == httpx.Limits(
        max_connections=100, max_keepalive_connections=20
    )


# ── 3. 非法值落在配置校验期 ────────────────────────────────────


@pytest.mark.parametrize("bad", [0, -3])
def test_non_positive_limit_is_rejected_at_parse_time(bad: int) -> None:
    """``0`` / 负数 → 配置校验报错（不是运行期构造 provider 时抛栈）。"""
    with pytest.raises(ValueError) as err:
        ProviderConfig(
            name="bad",
            base_url="https://x",
            api_key="k",
            models=["m"],
            max_connections=bad,
        )

    assert "max_connections" in str(err.value)


@pytest.mark.parametrize("bad", [0, -3])
def test_non_positive_limit_keeps_its_path(bad: int) -> None:
    """设置面（文档校验）拿到带精确路径的问题，而不是文档级兜底。"""
    raw = {
        "providers": [
            {
                "name": "p",
                "base_url": "https://x",
                "api_key": "k",
                "models": ["m"],
                "max_connections": bad,
            }
        ],
        "agents": [{"name": "a", "model": "m"}],
    }

    problems = locate_problems(raw)

    assert [p.path for p in problems] == ["providers[0].max_connections"]
    assert "greater than 0" in problems[0].message
