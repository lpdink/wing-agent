"""测试共享的普通 helper（非 fixture）。

跨多个测试文件重复出现的构造与假对象放这里；按 pytest fixture 生命周期需要
setup/teardown 的东西放 `conftest.py`。本模块只被测试 import，不被收集。
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import struct
import time
from collections.abc import AsyncIterator, Callable, Iterator, Sequence
from datetime import date, datetime, time as day_time
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

from wing.audit.recorder import AuditRecorder
from wing.chain import TrackedList
from wing.config import ApiKeyEntry, AuthConfig, ProviderConfig
from wing.context import Compactor, ContextManager
from wing.event import (
    LLMCallMetricsEvent,
    ToolCallResultEvent,
    UserMessageAcceptedEvent,
)
from wing.provider.anthropic import AnthropicProvider
from wing.provider.openai import OpenAICompatProvider
from wing.runtime import WingRuntime
from wing.schema import (
    ChainNode,
    LLMResponse,
    LLMUsage,
    Message,
    TextBlock,
    Tool,
    ToolParam,
)
from wing.store.file import FileMessageLog

# ── 会话 / 上下文 ─────────────────────────────────────────────


def seed_messages(session: Any, *contents: str) -> None:
    """向会话上下文写入若干消息（不经 LLM）：偶数下标 = user，奇数 = assistant。"""
    for i, content in enumerate(contents):
        role = "user" if i % 2 == 0 else "assistant"
        session.context_manager.add_message(Message(role=role, content=content))


def default_compactor() -> Compactor:
    """测试默认压缩器（窗口 100k / 保留 20k）。"""
    return Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)


def make_cm(
    tmp_dir: Path,
    *,
    session_id: str = "test-session",
    compactor: Any,
    system_prompt: str = "You are a helpful assistant.",
) -> ContextManager:
    """在 `tmp_dir` 下落一个 FileMessageLog，装配一个测试用 ContextManager。

    `compactor` 原样透传（含 `None` = 不要 compactor）——各测试对「缺省」的定义不同，
    默认值留在各自的 `_make_cm` 里。
    """
    messages: TrackedList[ChainNode] = TrackedList(FileMessageLog(tmp_dir / session_id))
    return ContextManager(
        session_id=session_id,
        messages=messages,
        system_prompt=system_prompt,
        compactor=compactor,
    )


def make_tool(
    name: str,
    fn: Callable[..., Any],
    *,
    description: str | None = None,
    params: Sequence[ToolParam] | None = None,
    namespace: str = "default",
) -> Tool:
    """最小化 Tool；`params` 缺省 = 一个名为 `input` 的字符串参数。"""
    return Tool(
        name=name,
        namespace=namespace,
        description=description or f"Test tool: {name}",
        params=(
            list(params)
            if params is not None
            else [ToolParam(name="input", type="string", description="input")]
        ),
        function=fn,
    )


# ── provider / SSE ────────────────────────────────────────────


def openai_sse(chunks: list[dict[str, Any]], *, done: bool = True) -> list[str]:
    """OpenAI 风格 SSE 行；`done=True` 结尾补 `data: [DONE]`（终态帧）。"""
    lines: list[str] = []
    for chunk in chunks:
        lines.append(f"data: {json.dumps(chunk)}")
        lines.append("")
    if done:
        lines.append("data: [DONE]")
        lines.append("")
    return lines


def anthropic_sse(events: list[dict[str, Any]]) -> list[str]:
    """Anthropic 风格 SSE 行（`event:` + `data:`）。"""
    lines: list[str] = []
    for event in events:
        lines.append(f"event: {event.get('type', 'unknown')}")
        lines.append(f"data: {json.dumps(event)}")
        lines.append("")
    return lines


def openai_provider(
    name: str, *, base_url: str = "https://api.example.com", **config_kwargs: object
) -> OpenAICompatProvider:
    """指向假 base_url 的 OpenAI 兼容 provider（不发真请求）。"""
    return OpenAICompatProvider(
        ProviderConfig(
            name=name,
            protocol="openai",
            base_url=base_url,
            api_key="sk-test",
            **config_kwargs,  # ty: ignore[invalid-argument-type]
        )
    )


def anthropic_provider(
    name: str, *, base_url: str = "https://api.anthropic.com", **config_kwargs: object
) -> AnthropicProvider:
    """指向假 base_url 的 Anthropic provider（不发真请求）。"""
    return AnthropicProvider(
        ProviderConfig(
            name=name,
            protocol="anthropic",
            base_url=base_url,
            api_key="sk-test",
            **config_kwargs,  # ty: ignore[invalid-argument-type]
        )
    )


async def mock_generate(*args: Any, **kwargs: Any) -> AsyncIterator[LLMResponse]:
    """Mock LLM generate：一条纯文本响应（无 tool calls），结束 turn。

    最终 chunk 必须携带权威 `content_blocks`——无效轮次会触发重试。
    """
    yield LLMResponse(
        content="ok",
        content_blocks=[TextBlock(text="ok")],
        usage=LLMUsage(prompt_tokens=10, completion_tokens=5),
    )


# ── 轮询 ──────────────────────────────────────────────────────


async def wait_until_async(pred: Callable[[], bool], timeout: float = 5.0) -> None:
    """轮询等待 `pred()` 为真（超时抛 `TimeoutError`）。"""

    async def _inner() -> None:
        while not pred():
            await asyncio.sleep(0.005)

    await asyncio.wait_for(_inner(), timeout)


def wait_until_sync(pred: Callable[[], bool], timeout: float = 3.0) -> bool:
    """轮询等待 `pred()` 为真；超时返回最后一次求值结果。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if pred():
            return True
        time.sleep(0.01)
    return pred()


# ── 媒体 ──────────────────────────────────────────────────────


def png_bytes(width: int = 4, height: int = 4, payload: bytes = b"") -> bytes:
    """最小 PNG 前缀 + 可选载荷：只求字节事实（magic / 头部尺寸 / sha256），不真解码。"""
    return (
        b"\x89PNG\r\n\x1a\n"
        + struct.pack(">I", 13)
        + b"IHDR"
        + struct.pack(">II", width, height)
        + b"\x08\x06\x00\x00\x00"
        + payload
    )


#: 最小 PNG 头（sig + IHDR 长度/类型/宽高/位深色型压缩滤波隔行）的字节数。
PNG_HEADER_LEN = len(png_bytes())


# ── gateway ───────────────────────────────────────────────────


def gateway_auth_config(
    auth_enabled: bool = False, keys: list[ApiKeyEntry] | None = None
) -> MagicMock:
    """只填 `gateway.auth` 的 mock Config（`GatewayServer.auth_config` 读它）。"""
    config = MagicMock()
    config.gateway.auth = AuthConfig(enabled=auth_enabled, keys=keys or [])
    return config


@contextlib.contextmanager
def gateway_server(tmp_path: Path, monkeypatch: Any) -> Iterator[Any]:
    """独立 sessions 根 + 真实 `WingRuntime` 的 `GatewayServer`（`load_config` 打替身）。

    鉴权关闭；其余配置字段保持 `MagicMock` 的宽容行为。不起 uvicorn——调用方自己套
    `TestClient`，并按需包装 ASGI app。**sessions 根 = `tmp_path / "sessions"`**（本函数
    设置 `WING_SESSIONS_PATH`）；调用方要断言落点请用 `sessions_root()`，不要自己拼。
    """
    from wing.gateway.server import GatewayServer

    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    with patch("wing.gateway.server.load_config") as mock_load_config:
        mock_load_config.return_value = gateway_auth_config()
        server = GatewayServer()
    server.runtime = WingRuntime()
    yield server


def sessions_root() -> Path:
    """当前测试的 sessions 根（与 runtime 构造时同一个配置解析口）。"""
    from wing.config import get_config

    return get_config().sessions.resolved_path()


class BodyWatcher:
    """ASGI 包装：记录下游从 `receive` 拉走的每一条消息（「先校验、后收包」的观测面）。"""

    def __init__(self, app: Any) -> None:
        self.app = app
        self.messages: list[dict[str, Any]] = []

    async def __call__(self, scope: Any, receive: Any, send: Any) -> None:
        async def counting_receive() -> Any:
            message = await receive()
            self.messages.append(message)
            return message

        await self.app(scope, counting_receive, send)

    def request_bodies(self) -> list[bytes]:
        return [
            m.get("body", b"") for m in self.messages if m["type"] == "http.request"
        ]


def media_dir(session_id: str) -> Path:
    """会话的媒体对象目录（`<sessions-root>/<sid>/media`）。"""
    return sessions_root() / session_id / "media"


# ── 审计播种（用量统计类测试共用）────────────────────────────
#
# 一律走生产写入路径 `AuditRecorder`：测的是「事件 → 行 → 聚合」这条链，不是 SQLite。


def local_time(day: date, hour: int = 12) -> datetime:
    """某天某点的**本地 naive** 时刻（审计事件的 `created_at`，即事实发生时刻）。"""
    return datetime.combine(day, day_time(hour, 0))


def seed_llm_call(
    session_id: str,
    at: datetime,
    *,
    model: str = "gpt-4",
    provider: str = "default",
    prompt: int = 100,
    completion: int = 10,
    cached: int = 40,
    duration_ms: int = 500,
) -> None:
    """写一行 `audit_llm_call`（带使用量的一帧）。"""
    AuditRecorder().on_event(
        LLMCallMetricsEvent(
            session_id=session_id,
            request_id="req",
            model=model,
            provider=provider,
            prompt_tokens=prompt,
            completion_tokens=completion,
            cached_tokens=cached,
            first_chunk_rt_ms=12.0,
            tokens_per_sec=30.0,
            duration_ms=duration_ms,
            created_at=at,
        )
    )


def seed_tool_call(
    session_id: str,
    at: datetime,
    *,
    tool_name: str = "Bash",
    success: bool = True,
    duration_ms: int = 30,
) -> None:
    """写一行 `audit_tool_call`（一次真实执行）。"""
    AuditRecorder().on_event(
        ToolCallResultEvent(
            session_id=session_id,
            request_id="req",
            tool_name=tool_name,
            tool_args={},
            tool_call_id="tc",
            tool_result="ok",
            tool_success=success,
            duration_ms=duration_ms,
            created_at=at,
        )
    )


def seed_user_message(session_id: str, at: datetime) -> None:
    """写一行 `audit_user_message`（一次被接受的用户输入）。"""
    AuditRecorder().on_event(
        UserMessageAcceptedEvent(session_id=session_id, content="hi", created_at=at)
    )
