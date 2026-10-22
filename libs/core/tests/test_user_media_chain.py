"""用户消息携带媒体（贴图）链路的单测。

覆盖 step 02 message_chain 的服务端语义：

- 上行链路透传：`runtime.post → SM._post → Session.post → Agent.post → Inbox.post`
  把 `media` 带进 `Message`；
- 消息入口校验：id 格式 + 对象存在（`media_exists`），幽灵引用 / 非法 id 一律
  拒绝（`InvalidInputError`，且**没有**送达 ack、不进队列）；
- `run_turn` 合并：media 按「消息序 × 消息内序」concat；纯图消息合法；
  `user_message_accepted` 条件放宽（content 或 media）；
- steer 部分 drain（D7）：队首连续无 media 纯文本才可 steer，首条带图消息起
  全部留队列（保序、不丢）；
- 首条纯图消息不写入空标题。

图片字节用最小 PNG 头即可（本文件不真解码，只对账引用与顺序）。
"""

from __future__ import annotations

import asyncio
import hashlib
from typing import Any
from unittest.mock import patch

import pytest

from _support import mock_generate, png_bytes
from wing.common.utils import InvalidInputError
from wing.event import DeliveredEvent, UserMessageAcceptedEvent
from wing.event_bus import event_bus
from wing.gateway.protocol import ClientRequest
from wing.gateway.protocol.session import SendMessageRequest
from wing.schema import LLMResponse, LLMUsage, MediaRef, TextBlock

pytestmark = pytest.mark.usefixtures("cleanup_event_bus")


@pytest.fixture
def runtime():
    """创建 WingRuntime 实例。"""
    from wing.runtime import WingRuntime

    return WingRuntime()


def make_ref(data: bytes, name: str = "a.png") -> MediaRef:
    return MediaRef(
        id=hashlib.sha256(data).hexdigest(),
        mime="image/png",
        bytes=len(data),
        width=4,
        height=4,
        name=name,
    )


def seed(session: Any, data: bytes) -> MediaRef:
    """把对象写进会话媒体目录（存储层冻结接口），返回对应引用。"""
    ref = make_ref(data)
    session.store.write_media(session.session_id, ref.id, data)
    return ref


async def _stop_worker(agent: Any) -> None:
    """停止自动 worker，手动控制处理时机。"""
    agent._worker.cancel()
    try:
        await agent._worker
    except (Exception, asyncio.CancelledError):
        pass


def _put(agent: Any, content: str, request_id: str | None = None, media=None) -> None:
    from wing.agent.inbox import Inbound
    from wing.schema import Message

    agent._inbox._queue.put_nowait(
        Inbound(
            message=Message(role="user", content=content, media=media),
            request_id=request_id,
        )
    )


class TestUplinkPlumbing:
    """六层透传：media 从 runtime.post 一路进 Inbox 的 Message。"""

    @pytest.mark.asyncio
    async def test_post_carries_media_into_inbox_message(self, runtime: Any):
        session = runtime.create_session()
        await _stop_worker(session.agent)
        ref = seed(session, png_bytes(payload=b"plumb"))

        await runtime.post(
            "look", request_id="req-1", session_id=session.session_id, media=[ref]
        )

        queued = session.agent._inbox.drain()
        assert len(queued) == 1, queued
        assert queued[0].message.content == "look"
        assert [r.id for r in queued[0].message.media or []] == [ref.id]

    @pytest.mark.asyncio
    async def test_post_without_media_keeps_message_clean(self, runtime: Any):
        """无 media 路径：Message.media 保持 None（逐字节兼容旧行为）。"""
        session = runtime.create_session()
        await _stop_worker(session.agent)

        await runtime.post("plain", request_id="req-1", session_id=session.session_id)

        queued = session.agent._inbox.drain()
        assert queued[0].message.media is None

    @pytest.mark.asyncio
    async def test_empty_media_list_normalizes_to_none(self, runtime: Any):
        """显式空列表 = 无媒体（Message validator 归一，不落 `media: []`）。"""
        session = runtime.create_session()
        await _stop_worker(session.agent)

        await runtime.post("x", session_id=session.session_id, media=[])

        assert session.agent._inbox.drain()[0].message.media is None


class TestMediaValidation:
    """消息入口闸门：幽灵引用 / 非法 id 拒绝，且拒绝发生在送达 ack 之前。"""

    @staticmethod
    def _delivered(events: list[Any]) -> list[Any]:
        return [e for e in events if isinstance(e, DeliveredEvent)]

    @pytest.mark.asyncio
    async def test_ghost_ref_is_rejected_before_delivery(self, runtime: Any):
        session = runtime.create_session()
        await _stop_worker(session.agent)
        events: list[Any] = []
        event_bus.subscribe(events.append)
        ghost = make_ref(png_bytes(payload=b"ghost"))  # 格式合法但对象不存在

        with pytest.raises(InvalidInputError) as excinfo:
            await runtime.post("x", session_id=session.session_id, media=[ghost])

        message = str(excinfo.value)
        assert "not found" in message and ghost.id[:8] in message, message
        assert session.agent._inbox.has_pending is False
        assert self._delivered(events) == []

    @pytest.mark.asyncio
    async def test_malformed_id_is_rejected(self, runtime: Any):
        session = runtime.create_session()
        await _stop_worker(session.agent)
        bad = make_ref(png_bytes(payload=b"bad")).model_copy(
            update={"id": "not-a-hash"}
        )

        with pytest.raises(InvalidInputError) as excinfo:
            await runtime.post("x", session_id=session.session_id, media=[bad])

        assert "invalid media reference" in str(excinfo.value)
        assert session.agent._inbox.has_pending is False

    @pytest.mark.asyncio
    async def test_unsupported_mime_is_rejected(self, runtime: Any):
        """mime 不在读图支持列表内：即使对象存在也拒（mime 原样进线格式）。"""
        session = runtime.create_session()
        await _stop_worker(session.agent)
        ref = seed(session, png_bytes(payload=b"mime")).model_copy(
            update={"mime": "image/svg+xml"}
        )

        with pytest.raises(InvalidInputError) as excinfo:
            await runtime.post("x", session_id=session.session_id, media=[ref])

        assert "unsupported media reference mime" in str(excinfo.value)
        assert session.agent._inbox.has_pending is False

        # mime 闸门先于存在性查询（ghost id + 非法 mime → 报 mime，不报 not found）
        both_bad = make_ref(png_bytes(payload=b"both")).model_copy(
            update={"id": "c" * 64, "mime": "text/plain"}
        )
        with pytest.raises(InvalidInputError) as excinfo:
            await runtime.post("x", session_id=session.session_id, media=[both_bad])
        assert "unsupported media reference mime" in str(excinfo.value)

    @pytest.mark.asyncio
    async def test_existing_ref_passes_and_delivers(self, runtime: Any):
        session = runtime.create_session()
        await _stop_worker(session.agent)
        ref = seed(session, png_bytes(payload=b"ok"))
        events: list[Any] = []
        event_bus.subscribe(events.append)

        await runtime.post("x", session_id=session.session_id, media=[ref])

        assert len(self._delivered(events)) == 1
        assert session.agent._inbox.has_pending is True

    @pytest.mark.asyncio
    async def test_unknown_session_still_raises_lookup_error(self, runtime: Any):
        """会话不存在优先于媒体校验（归因正确：先解决 session）。"""
        with pytest.raises(LookupError):
            await runtime.post(
                "x", session_id="definitely-not-exists", media=[make_ref(b"x")]
            )


class TestTurnMerge:
    """run_turn 的 media 合并与纯图消息语义。"""

    @pytest.mark.asyncio
    async def test_media_concat_in_message_order(self, runtime: Any):
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)
        r1, r2 = make_ref(png_bytes(payload=b"1")), make_ref(png_bytes(payload=b"2"))
        r3 = make_ref(png_bytes(payload=b"3"))

        _put(agent, "A", "req-1", media=[r1, r2])
        _put(agent, "B", "req-2", media=[r3])

        with patch.object(agent.model_provider, "generate", mock_generate):
            await agent._loop.run_turn()

        user_msgs = [
            m for m in agent.context_manager._messages.active_chain if m.role == "user"
        ]
        assert len(user_msgs) == 1
        assert user_msgs[0].content == "A\nB"
        assert [r.id for r in user_msgs[0].media or []] == [r1.id, r2.id, r3.id]

    @pytest.mark.asyncio
    async def test_pure_image_message_is_legal(self, runtime: Any):
        """content 为空、media 非空：消息进链、turn 正常结束、accepted 发射。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)
        ref = make_ref(png_bytes(payload=b"pure"))

        _put(agent, "", "req-img", media=[ref])
        events: list[Any] = []
        event_bus.subscribe(events.append)

        with patch.object(agent.model_provider, "generate", mock_generate):
            await agent._loop.run_turn()

        user_msgs = [
            m for m in agent.context_manager._messages.active_chain if m.role == "user"
        ]
        assert len(user_msgs) == 1, "纯图消息必须进上下文"
        assert user_msgs[0].content == ""
        assert [r.id for r in user_msgs[0].media or []] == [ref.id]

        accepted = [e for e in events if isinstance(e, UserMessageAcceptedEvent)]
        assert [(e.origin_request_id, e.content) for e in accepted] == [
            ("req-img", "")
        ], "纯图消息也要发射 accepted（否则 pending 卡到 turn end）"

    @pytest.mark.asyncio
    async def test_empty_post_still_returns_early(self, runtime: Any):
        """content 与 media 都空：仍直接返回（无消息进链、无 accepted）。"""
        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)

        _put(agent, "", "req-empty", media=None)
        events: list[Any] = []
        event_bus.subscribe(events.append)

        with patch.object(agent.model_provider, "generate", mock_generate):
            await agent._loop.run_turn()

        assert [
            m for m in agent.context_manager._messages.active_chain if m.role == "user"
        ] == []
        assert not any(isinstance(e, UserMessageAcceptedEvent) for e in events)


class TestSteerPartialDrain:
    """D7：steer 只取队首连续的无 media 纯文本，其余原序留队列。"""

    def _inbox(self):
        from wing.agent.inbox import Inbox
        from wing.schema import Message

        inbox = Inbox()
        return inbox, Message

    def _enqueue(self, inbox: Any, items: list[tuple[str, list[MediaRef] | None]]):
        from wing.agent.inbox import Inbound
        from wing.schema import Message

        for content, media in items:
            inbox._queue.put_nowait(
                Inbound(
                    message=Message(role="user", content=content, media=media),
                    request_id=f"req-{content}",
                )
            )

    def test_text_prefix_steers_tail_stays(self):
        inbox, _ = self._inbox()
        ref = make_ref(png_bytes(payload=b"steer"))
        self._enqueue(inbox, [("A", None), ("B", [ref]), ("C", None)])

        steered = inbox.drain_for_steer()

        assert [b.message.content for b in steered] == ["A"]
        rest = inbox.drain()
        assert [b.message.content for b in rest] == ["B", "C"], "保序且不丢"
        assert rest[0].message.media is not None, "带图消息的引用原样留在队列"

    def test_all_text_is_steerable(self):
        inbox, _ = self._inbox()
        self._enqueue(inbox, [("A", None), ("B", None)])

        steered = inbox.drain_for_steer()

        assert [b.message.content for b in steered] == ["A", "B"]
        assert inbox.has_pending is False

    def test_image_at_head_blocks_everything(self):
        inbox, _ = self._inbox()
        ref = make_ref(png_bytes(payload=b"head"))
        self._enqueue(inbox, [("B", [ref]), ("C", None)])

        assert inbox.drain_for_steer() == []
        assert [b.message.content for b in inbox.drain()] == ["B", "C"]

    def test_empty_content_entry_blocks_the_rest(self):
        """空 content（内部投递）同样阻断——队列语义是"按序取前缀"，不跳条。"""
        inbox, _ = self._inbox()
        self._enqueue(inbox, [("A", None), ("", None), ("C", None)])

        steered = inbox.drain_for_steer()

        assert [b.message.content for b in steered] == ["A"]
        assert [b.message.content for b in inbox.drain()] == ["", "C"]

    def test_empty_queue_is_noop(self):
        inbox, _ = self._inbox()
        assert inbox.drain_for_steer() == []

    @pytest.mark.asyncio
    async def test_steer_blocked_by_media_defers_to_next_turn(self, runtime: Any):
        """工具执行期间到达的带图消息：本轮不注入 note，下一轮完整进入。"""
        from wing.schema import Tool, ToolCall, ToolParam, ToolUseBlock

        session = runtime.create_session()
        agent = session.agent
        await _stop_worker(agent)
        ref = make_ref(png_bytes(payload=b"queued"))

        async def _echo(input: str = "") -> str:
            _put(agent, "steer me", "req-steer")
            _put(agent, "", "req-img", media=[ref])
            _put(agent, "after", "req-after")
            return "tool-result"

        agent._executor._tools = {
            "Echo": Tool(
                name="Echo",
                description="echo tool",
                params=[ToolParam(name="input", type="string", description="input")],
                function=_echo,
            ),
        }

        tc = ToolCall(id="call-1", name="Echo", arguments={"input": "x"})
        calls = iter(
            [
                LLMResponse(
                    tool_calls=[tc],
                    content_blocks=[
                        ToolUseBlock(id=tc.id, name=tc.name, input=tc.arguments)
                    ],
                    usage=LLMUsage(prompt_tokens=1, completion_tokens=1),
                ),
                LLMResponse(
                    content="done",
                    content_blocks=[TextBlock(text="done")],
                    usage=LLMUsage(prompt_tokens=1, completion_tokens=1),
                ),
                LLMResponse(
                    content="second",
                    content_blocks=[TextBlock(text="second")],
                    usage=LLMUsage(prompt_tokens=1, completion_tokens=1),
                ),
            ]
        )

        async def _generate(*args: Any, **kwargs: Any):
            yield next(calls)

        _put(agent, "go", "req-0")
        with patch.object(agent.model_provider, "generate", _generate):
            await agent._loop.run_turn()

            # 本轮：只有纯文本消息进了 steer note；带图消息与其后的文本留队列。
            tool_msgs = [
                m
                for m in agent.context_manager._messages.active_chain
                if m.role == "tool"
            ]
            assert tool_msgs[0].content == "[User steer note: steer me]\ntool-result"
            assert "after" not in tool_msgs[0].content
            assert agent._inbox.has_pending is True

            # 下一轮：带图消息 + 其后文本一起合并进上下文（顺序不变）。
            await agent._loop.run_turn()

        user_msgs = [
            m for m in agent.context_manager._messages.active_chain if m.role == "user"
        ]
        assert user_msgs[-1].content == "after"
        assert [r.id for r in user_msgs[-1].media or []] == [ref.id]


class TestProtocolMediaField:
    """协议模型的 media 字段契约（WS ``ClientRequest`` / HTTP ``SendMessageRequest``）。

    字段是**尾随可选字段**（缺省 None）：缺省与显式 null 在 wire 上必须无差别
    （旧客户端 / 旧帧零影响），空数组在协议层保形、进 ``Message`` 时归一为 None，
    带 media 的 dump 与 ``MediaRef.model_dump()`` 逐键相同（前端上行帧的镜像形状）。
    """

    MODELS = [ClientRequest, SendMessageRequest]

    @staticmethod
    def _without_request_id(dump: dict[str, Any]) -> dict[str, Any]:
        """去掉自动生成的 request_id（只有 ClientRequest 有，且是随机 uuid）。"""
        return {key: value for key, value in dump.items() if key != "request_id"}

    @pytest.mark.parametrize("model", MODELS)
    def test_absent_and_null_decode_identically(self, model: Any):
        absent = model.model_validate({"session_id": "s", "content": "c"})
        explicit = model.model_validate(
            {"session_id": "s", "content": "c", "media": None}
        )
        assert absent.media is None and explicit.media is None
        assert self._without_request_id(
            absent.model_dump()
        ) == self._without_request_id(explicit.model_dump())

    @pytest.mark.parametrize("model", MODELS)
    def test_absent_media_not_in_exclude_none_dump(self, model: Any):
        """exclude_none dump 里不出现 media（旧客户端的观感：字段不存在）。"""
        dump = model.model_validate({"session_id": "s", "content": "c"}).model_dump(
            exclude_none=True
        )
        assert "media" not in dump

    @pytest.mark.parametrize("model", MODELS)
    def test_empty_media_array_decodes_and_message_normalizes_to_none(self, model: Any):
        """空数组：协议层保形（[]），进 Message 边界归一为 None（"无媒体"只有一种表示）。"""
        from wing.schema import Message

        decoded = model.model_validate({"session_id": "s", "content": "c", "media": []})
        assert decoded.media == []
        assert Message(role="user", content="c", media=decoded.media).media is None

    @pytest.mark.parametrize("model", MODELS)
    def test_media_dump_matches_media_ref(self, model: Any):
        ref = make_ref(png_bytes(payload=b"proto"))
        raw = {"session_id": "s", "content": "c", "media": [ref.model_dump()]}

        decoded = model.model_validate(raw)

        assert decoded.media == [ref]
        assert decoded.model_dump()["media"] == [ref.model_dump()]
        # round-trip：dump 能原样解析回同一引用（上行帧 = 该 dump）。
        assert model.model_validate(decoded.model_dump()).media == [ref]


class TestFirstMessageTitle:
    """首条纯图消息不写入空标题（D8）。"""

    @pytest.mark.asyncio
    async def test_pure_image_first_message_leaves_title_unset(self, runtime: Any):
        session = runtime.create_session()
        await _stop_worker(session.agent)
        ref = seed(session, png_bytes(payload=b"title"))

        await runtime.post(
            "", request_id="req-img", session_id=session.session_id, media=[ref]
        )
        assert session.session_name is None, "空 content 不得固化成空标题"

        await runtime.post(
            "hello", request_id="req-text", session_id=session.session_id
        )
        assert session.session_name == "hello", "后续文本消息照常获得自动标题"
