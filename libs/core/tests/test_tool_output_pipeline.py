"""工具结构化结果（ToolOutput）管道单测。

覆盖链路：ToolExecutor → Message(content/media) → 事件 tool_media →
serialize_message → 落盘（引用式、无 base64）/加载 → TokenCounter 图像项 →
Session/WingAgent 的 MediaAccess 接线。
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
from typing import Any

import pytest
import pytest_asyncio

from wing.common.token_counter import TokenCounter
from wing.chain import TrackedList
from wing.event import ToolCallResultEvent
from wing.event_bus import event_bus
from wing.hooks import hooks
from wing.media import MediaAccess, estimate_image_tokens
from wing.schema import ChainNode, MediaRef, Message, Tool, ToolCall, ToolOutput
from wing.session import serialize_message
from wing.store import FileSessionStore
from wing.store.memory import MemoryMessageLog

_REF = MediaRef(
    id="ab" * 32,
    mime="image/png",
    bytes=1024,
    width=640,
    height=480,
    name="shot.png",
)


def _make_tool(name: str, fn: Any) -> Tool:
    return Tool(
        name=name,
        description=f"Test tool: {name}",
        params=[],
        function=fn,
    )


def _make_tool_call(tool_id: str = "call-1", name: str = "T") -> ToolCall:
    return ToolCall(id=tool_id, name=name, arguments={})


@pytest.fixture(autouse=True)
def cleanup_globals():
    """每个测试前后清理全局 EventBus 与 HookRegistry。"""
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    hooks.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    hooks.clear()


@pytest_asyncio.fixture
async def agent():
    """真实 runtime 里的 agent（tool executor 行为走真实装配路径）。"""
    from wing.runtime import WingRuntime

    session = WingRuntime().create_session()
    try:
        yield session.agent
    finally:
        await session.agent.shutdown()


########## ToolExecutor 管道


class TestExecutorPipeline:
    @pytest.mark.asyncio
    async def test_tool_output_flows_into_message(self, agent: Any):
        async def image_tool() -> ToolOutput:
            return ToolOutput(content="[image: /tmp/a.png | png 640x480]", media=[_REF])

        agent._executor._tools = {"T": _make_tool("T", image_tool)}
        results = await agent._executor.execute([_make_tool_call()], agent.model)

        assert len(results) == 1
        msg = results[0]
        assert msg.role == "tool"
        assert msg.tool_call_id == "call-1"
        assert msg.content == "[image: /tmp/a.png | png 640x480]"
        assert msg.media == [_REF]

    @pytest.mark.asyncio
    async def test_plain_str_result_has_no_media(self, agent: Any):
        """存量 str 返回值零变化：media 为 None（不是空列表）。"""

        async def plain_tool() -> str:
            return "plain result"

        agent._executor._tools = {"T": _make_tool("T", plain_tool)}
        results = await agent._executor.execute([_make_tool_call()], agent.model)

        assert results[0].content == "plain result"
        assert results[0].media is None

    @pytest.mark.asyncio
    async def test_empty_media_tool_output_is_normalized(self, agent: Any):
        async def empty_tool() -> ToolOutput:
            return ToolOutput(content="text only")

        agent._executor._tools = {"T": _make_tool("T", empty_tool)}
        results = await agent._executor.execute([_make_tool_call()], agent.model)

        assert results[0].content == "text only"
        assert results[0].media is None

    @pytest.mark.asyncio
    async def test_hook_sees_text_only_and_cannot_drop_media(self, agent: Any):
        hooks.on("after_tool_call")(lambda result, **ctx: f"[hooked] {result}")

        async def image_tool() -> ToolOutput:
            return ToolOutput(content="raw", media=[_REF])

        agent._executor._tools = {"T": _make_tool("T", image_tool)}
        results = await agent._executor.execute([_make_tool_call()], agent.model)

        assert results[0].content == "[hooked] raw"
        assert results[0].media == [_REF]

    @pytest.mark.asyncio
    async def test_truncation_applies_to_content_only(
        self,
        agent: Any,
        _mock_config: Any,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ):
        # 截断会把全文写进 WING_HOME/tmp——重定向到临时目录，别碰用户目录。
        monkeypatch.setenv("WING_HOME", str(tmp_path / "wing-home"))
        _mock_config.tool_result_truncate.max_length = 50
        _mock_config.tool_result_truncate.keep_chars = 5

        async def long_tool() -> ToolOutput:
            return ToolOutput(content="X" * 500, media=[_REF])

        agent._executor._tools = {"T": _make_tool("T", long_tool)}
        results = await agent._executor.execute([_make_tool_call()], agent.model)

        msg = results[0]
        assert "truncated" in msg.content
        assert len(msg.content) < 500
        assert msg.media == [_REF]  # media 是引用元数据，不截断

    @pytest.mark.asyncio
    async def test_tool_error_path_has_no_media(self, agent: Any):
        from wing.schema import ToolError

        async def failing_tool() -> ToolOutput:
            raise ToolError("nope")

        agent._executor._tools = {"T": _make_tool("T", failing_tool)}
        results = await agent._executor.execute([_make_tool_call()], agent.model)

        assert results[0].content == "nope"
        assert results[0].media is None


########## 事件


class TestToolEvent:
    @pytest.mark.asyncio
    async def test_event_carries_tool_media(self, agent: Any):
        events: list[ToolCallResultEvent] = []

        def on_event(event: Any) -> None:
            if isinstance(event, ToolCallResultEvent):
                events.append(event)

        event_bus.subscribe(on_event)

        async def image_tool() -> ToolOutput:
            return ToolOutput(content="envelope", media=[_REF])

        agent._executor._tools = {"T": _make_tool("T", image_tool)}
        await agent._executor.execute([_make_tool_call()], agent.model)

        assert len(events) == 1
        assert events[0].tool_media == [_REF.model_dump()]
        assert events[0].tool_result == "envelope"

    @pytest.mark.asyncio
    async def test_event_tool_media_defaults_empty(self, agent: Any):
        events: list[ToolCallResultEvent] = []

        def on_event(event: Any) -> None:
            if isinstance(event, ToolCallResultEvent):
                events.append(event)

        event_bus.subscribe(on_event)

        async def plain_tool() -> str:
            return "ok"

        agent._executor._tools = {"T": _make_tool("T", plain_tool)}
        await agent._executor.execute([_make_tool_call()], agent.model)

        assert events[0].tool_media == []


########## 序列化


class TestSerializeMessage:
    def test_media_present_as_references(self):
        msg = Message(role="tool", tool_call_id="c", content="x", media=[_REF])
        assert serialize_message(msg)["media"] == [_REF.model_dump()]

    def test_no_media_key_when_absent(self):
        msg = Message(role="tool", tool_call_id="c", content="x")
        assert "media" not in serialize_message(msg)

    def test_empty_media_normalized_to_none(self):
        msg = Message(role="tool", tool_call_id="c", content="x", media=[])
        assert msg.media is None
        assert "media" not in serialize_message(msg)


########## TokenCounter


class TestTokenCounterImages:
    def test_estimate_message_counts_images(self):
        msg = Message(role="tool", tool_call_id="c", content="x", media=[_REF, _REF])
        expected = TokenCounter.count(repr(msg)) + 2 * estimate_image_tokens(_REF)
        assert TokenCounter.estimate_message(msg) == expected

    def test_estimate_message_without_media_unchanged(self):
        msg = Message(role="tool", tool_call_id="c", content="x")
        assert TokenCounter.estimate_message(msg) == TokenCounter.count(repr(msg))


########## 落盘 / 加载


class TestPersistence:
    def test_record_includes_media_reference_only(self):
        log = MemoryMessageLog()
        tracked: TrackedList[ChainNode] = TrackedList(log)
        tracked.append(
            Message(role="tool", tool_call_id="c", content="x", media=[_REF])
        )

        records = list(log.iter_all())
        assert len(records) == 1
        record = records[0]
        assert record["media"] == [_REF.model_dump()]
        # 引用式：绝无 base64（字节在 SessionStore），记录里只有 id/元数据
        dumped = json.dumps(record)
        assert "base64" not in dumped
        assert _REF.id in dumped

    def test_record_omits_media_key_when_none(self):
        log = MemoryMessageLog()
        tracked: TrackedList[ChainNode] = TrackedList(log)
        tracked.append(Message(role="user", content="hi"))

        record = list(log.iter_all())[0]
        assert "media" not in record

    def test_roundtrip_load_preserves_media(self):
        log = MemoryMessageLog()
        tracked: TrackedList[ChainNode] = TrackedList(log)
        tracked.append(
            Message(role="tool", tool_call_id="c", content="x", media=[_REF])
        )
        tracked.extend([Message(role="assistant", content="ok")])

        loaded = TrackedList.load(log, Message)
        msgs = [m for m in loaded.active_chain if isinstance(m, Message)]
        assert msgs[0].media == [_REF]
        assert msgs[1].media is None

    def test_legacy_record_without_media_loads(self):
        """旧会话（无 media 键）必须照常加载。"""
        log = MemoryMessageLog()
        log.append(
            [{"role": "tool", "content": "old", "tool_call_id": "c", "uuid": "u1"}]
        )
        loaded = TrackedList.load(log, Message)
        msgs = [m for m in loaded.active_chain if isinstance(m, Message)]
        assert len(msgs) == 1
        assert msgs[0].content == "old"
        assert msgs[0].media is None

    def test_message_validate_without_media_key(self):
        msg = Message.model_validate({"role": "user", "content": "hi"})
        assert msg.media is None

    def test_assistant_message_accepts_media_field(self):
        """assistant 消息也接受 media 字段（预留 user/assistant 的形态宽容）。"""
        msg = Message(role="assistant", content="hi", media=[_REF])
        assert msg.media == [_REF]
        assert msg.content == "hi"


########## Session / Agent 接线


class TestSessionWiring:
    @pytest.mark.asyncio
    async def test_session_agent_exposes_media_access(self):
        from wing.runtime import WingRuntime

        session = WingRuntime().create_session(backend="memory")
        agent = session.agent
        try:
            assert isinstance(agent.media, MediaAccess)
            media = agent.media
            payload = b"img-bytes"
            mid = hashlib.sha256(payload).hexdigest()
            media.write(mid, payload)
            assert media.read(mid) == payload
            assert media.read("cd" * 32) is None

            # 初始 provider 与切换/懒创建的 provider 都拿到同一 MediaAccess
            assert agent.model_provider._media is agent.media
            alt = agent.get_or_create_provider("alt")
            assert alt._media is agent.media
        finally:
            await agent.shutdown()

    @pytest.mark.asyncio
    async def test_media_access_wired_to_store_root(self):
        """file 后端：MediaAccess 的读写落到 WING_SESSIONS_PATH 的 .media 池。"""
        from wing.runtime import WingRuntime

        session = WingRuntime().create_session()
        agent = session.agent
        try:
            media = agent.media
            assert media is not None
            payload = b"png-bytes"
            mid = hashlib.sha256(payload).hexdigest()
            media.write(mid, payload)
            root = Path(os.environ["WING_SESSIONS_PATH"])
            assert (root / ".media" / mid[:2] / mid).read_bytes() == payload
            assert FileSessionStore(root).read_media(mid) == payload
        finally:
            await agent.shutdown()
