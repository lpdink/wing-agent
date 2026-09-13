"""网关 WS 帧切分契约：软上限切分 / 硬上限丢弃 / 信封 / 帧顺序。

背景（2026-09-13 实测）：客户端单帧上限 16 MiB（tungstenite 默认，不动），而
`sync_session` 载荷可达 22,151,988 B → 读任务死亡 → 重连重同步再死，会话在 TUI
里永久打不开。上界必须由发送方保证：超过软上限（8 MiB）的载荷切分为 N 帧，
每帧 ≤ 软上限；仍超硬上限（16 MiB）的帧被拦截丢弃 + 一行 WARN。

本文件钉死：
- 切分边界（恰好 ≤ 软上限 / 略超 / 多字节字符边界 / 帧数最小化）；
- 信封形状稳定（六字段、index 连续、同一事件共享 id、of_type 正确）；
- 同一事件的帧在一个发送任务内按序发出（`_send_frames`），全局与定向同路径；
- 硬上限丢弃 + 恰一行 WARN，其余帧照常投递；写失败/超时后剩余帧不再尝试；
- 小事件路径与既有一致（单帧、内容逐字节不变）。
"""

from __future__ import annotations

import asyncio
import json
from typing import cast
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi import WebSocket

from wing.config import ApiKeyEntry, AuthConfig
from wing.event import EventTarget, NoticeEvent, wire_dump
from wing.event_bus import event_bus
from wing.gateway.frames import (
    HARD_LIMIT_BYTES,
    SOFT_LIMIT_BYTES,
    build_frames,
)

SESSION = "test-session"


def _as_ws(obj: object) -> WebSocket:
    """测试替身冒充 `WebSocket`（运行时是鸭子类型，类型上显式断言）。"""
    return cast("WebSocket", obj)


def _mock_config() -> MagicMock:
    config = MagicMock()
    config.gateway.auth = AuthConfig(
        enabled=False, keys=[ApiKeyEntry(key="k", role="admin")]
    )
    return config


def _make_server():
    """造一个 GatewayServer（WingRuntime / load_config 都 mock 掉）。"""
    with (
        patch("wing.gateway.server.WingRuntime") as MockRuntime,
        patch("wing.gateway.server.load_config") as mock_load_config,
    ):
        MockRuntime.return_value = MagicMock()
        mock_load_config.return_value = _mock_config()
        from wing.gateway.server import GatewayServer

        return GatewayServer()


@pytest.fixture(autouse=True)
def clean_event_bus():
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


class _RecordingWs:
    """记录所有出网文本的客户端替身。"""

    def __init__(self, *, fail_after: int | None = None) -> None:
        self.sent: list[str] = []
        self.close_calls: list[tuple[int, str]] = []
        self._fail_after = fail_after

    async def send_text(self, data: str) -> None:
        if self._fail_after is not None and len(self.sent) >= self._fail_after:
            raise RuntimeError(
                "Unexpected ASGI message 'websocket.send', after sending 'websocket.close'"
            )
        self.sent.append(data)

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self.close_calls.append((code, reason))


class _HangingWs:
    """第一帧就永不返回（慢/死消费者）。"""

    def __init__(self) -> None:
        self.sent: list[str] = []

    async def send_text(self, data: str) -> None:
        await asyncio.Event().wait()

    async def close(self, code: int = 1000, reason: str = "") -> None:
        pass


def _register(server, ws, client_id: str = "client-1") -> None:
    server.clients[client_id] = ws
    server.ws_to_clients[ws] = client_id


def _notice(message: str) -> NoticeEvent:
    """带 global target 的事件（EventBus 投递时才注入 target）。"""
    return NoticeEvent(message=message, target=EventTarget(scope="global"))


def _payload_of(message: str) -> str:
    return json.dumps(wire_dump(_notice(message)))


# ============================================================
# 纯函数：切分边界
# ============================================================


class TestBuildFrames:
    def test_small_payload_is_single_frame(self):
        payload = _payload_of("hello")
        frames = build_frames(payload, "notice")
        assert len(frames) == 1
        assert frames[0].text == payload, "小载荷必须原样单帧（不包装）"
        assert frames[0].size == len(payload.encode("utf-8"))

    def test_exact_soft_limit_is_single_frame(self):
        payload = "x" * SOFT_LIMIT_BYTES
        frames = build_frames(payload, "sync_session")
        assert len(frames) == 1, "恰好等于软上限不切分"
        assert frames[0].text == payload

    def test_one_byte_over_soft_limit_splits(self):
        payload = "x" * (SOFT_LIMIT_BYTES + 1)
        frames = build_frames(payload, "sync_session")
        assert len(frames) == 2
        assert "".join(json.loads(f.text)["data"] for f in frames) == payload

    def test_frames_are_within_soft_limit_and_full(self):
        """20 MiB → 3 帧；每帧 ≤ 软上限，且尽量撞软上限（不浪费帧数）。"""
        payload = "x" * (20 * 1024 * 1024)
        frames = build_frames(payload, "sync_session")
        assert len(frames) == 3
        for frame in frames:
            assert frame.size <= SOFT_LIMIT_BYTES
            assert frame.size == len(frame.text.encode("utf-8")), "size 必须精确"
        for frame in frames[:-1]:
            assert frame.size >= SOFT_LIMIT_BYTES - 4096, (
                f"帧太短，浪费帧数：{frame.size}"
            )

    @pytest.mark.parametrize("unit", ["中", "🙂"])
    def test_multibyte_boundary_never_splits_a_character(self, unit: str):
        """软上限正好落在字符内部字节位置时，切点必须回退到字符边界。"""
        payload = unit * (3 * 1024 * 1024)
        frames = build_frames(payload, "sync_session")
        assert len(frames) >= 2
        joined = ""
        for frame in frames:
            # 每帧的 data 必须是合法 UTF-8（JSON 解析本身就会校验）
            joined += json.loads(frame.text)["data"]
        assert joined == payload

    def test_escaping_is_accounted_for(self):
        """引号/反斜杠会 JSON 转义膨胀——预算必须按转义后长度算。"""
        payload = '"' * (12 * 1024 * 1024)
        frames = build_frames(payload, "sync_session")
        for frame in frames:
            assert frame.size <= SOFT_LIMIT_BYTES
        first = frames[0]
        assert first.size >= SOFT_LIMIT_BYTES - 4096, (
            f"转义预算算错会浪费一半帧：{first.size}"
        )

    def test_raw_control_characters_still_respect_the_limit(self):
        """载荷含裸控制字符（不是 `json.dumps` 输出）时退回精确测量，仍不超限。

        `json.dumps` 的输出里控制字符必然是转义形态（常态走转义计数的快路径）；
        这里刻意构造裸控制字符，钉住慢路径的存在与正确性。
        """
        payload = 'a\u0001b\\c"d' * (2 * 1024 * 1024)
        frames = build_frames(payload, "sync_session")
        assert len(frames) >= 2
        for frame in frames:
            assert frame.size <= SOFT_LIMIT_BYTES
            assert frame.size == len(frame.text.encode("utf-8"))
        joined = "".join(json.loads(f.text)["data"] for f in frames)
        assert joined == payload

    def test_envelope_fields_are_stable(self):
        payload = "x" * (SOFT_LIMIT_BYTES + 1)
        frames = build_frames(payload, "sync_session")
        envelopes = [json.loads(f.text) for f in frames]
        assert [set(e) for e in envelopes] == [
            {"type", "id", "index", "count", "of_type", "data"}
        ] * len(envelopes)
        assert {e["type"] for e in envelopes} == {"_chunk"}
        assert {e["of_type"] for e in envelopes} == {"sync_session"}
        assert [e["index"] for e in envelopes] == list(range(len(envelopes)))
        assert {e["count"] for e in envelopes} == {len(envelopes)}
        assert len({e["id"] for e in envelopes}) == 1, "同一事件共享 id"

    def test_chunk_ids_are_not_reused_across_events(self):
        payload = "x" * (SOFT_LIMIT_BYTES + 1)
        first = {json.loads(f.text)["id"] for f in build_frames(payload, "notice")}
        second = {json.loads(f.text)["id"] for f in build_frames(payload, "notice")}
        assert first.isdisjoint(second)

    def test_limits_are_constants(self):
        assert SOFT_LIMIT_BYTES == 8 * 1024 * 1024
        assert HARD_LIMIT_BYTES == 16 * 1024 * 1024, (
            "硬上限必须等于客户端 tungstenite 默认 max_frame_size"
        )


# ============================================================
# 网关投递：顺序 / 全局与定向 / 硬上限丢弃 / 回收后停止
# ============================================================


@pytest.mark.asyncio
class TestChunkedDelivery:
    async def test_oversized_event_is_sent_as_ordered_frames(self, monkeypatch):
        monkeypatch.setattr("wing.gateway.frames.SOFT_LIMIT_BYTES", 512)
        server = _make_server()
        stub = _RecordingWs()
        ws = _as_ws(stub)
        _register(server, ws)

        event = _notice("x" * 2000)
        payload = json.dumps(wire_dump(event))
        server._on_event(event)
        await asyncio.sleep(0.05)

        assert len(stub.sent) >= 2, f"超限载荷必须切分：{len(stub.sent)}"
        envelopes = [json.loads(t) for t in stub.sent]
        assert [e["index"] for e in envelopes] == list(range(len(envelopes)))
        assert {e["id"] for e in envelopes} == {envelopes[0]["id"]}
        assert "".join(e["data"] for e in envelopes) == payload

    async def test_global_and_client_scope_share_the_path(self, monkeypatch):
        monkeypatch.setattr("wing.gateway.frames.SOFT_LIMIT_BYTES", 512)
        # 两个独立网关：一个走 global 广播，一个走 client 定向（避免互相干扰）。
        global_server, client_server = _make_server(), _make_server()
        global_stub, client_stub = _RecordingWs(), _RecordingWs()
        _register(global_server, _as_ws(global_stub), "global-client")
        _register(client_server, _as_ws(client_stub), "client-client")

        payload = "x" * 2000
        # 两条路径必须产出同一批帧：把 wire 形状钉成同一份（事件元数据会带上
        # 各自的时间戳 / request_id，否则无法逐帧对比）。
        monkeypatch.setattr(
            "wing.gateway.server.wire_dump",
            lambda _event: {"type": "notice", "message": payload},
        )
        global_server._on_event(_notice(payload))
        client_server._on_event(
            NoticeEvent(
                message=payload,
                target=EventTarget(scope="client", client_ids=["client-client"]),
            )
        )
        await asyncio.sleep(0.05)

        def shape(sent: list[str]) -> list[dict]:
            # `id` 按事件生成，逐事件不同；其余帧形状必须一致。
            return [{k: v for k, v in json.loads(t).items() if k != "id"} for t in sent]

        assert len(global_stub.sent) == len(client_stub.sent) >= 2
        assert shape(global_stub.sent) == shape(client_stub.sent), (
            "两条投递路径必须同帧同序"
        )

    async def test_oversized_frame_is_dropped_with_one_warning(self, monkeypatch):
        """硬上限是最终契约：仍超限的帧被拦截（每帧恰一行 WARN），其余帧照常投递。"""
        soft, hard = 300, 250
        monkeypatch.setattr("wing.gateway.frames.SOFT_LIMIT_BYTES", soft)
        monkeypatch.setattr("wing.gateway.server.HARD_LIMIT_BYTES", hard)
        server = _make_server()
        stub = _RecordingWs()
        _register(server, _as_ws(stub), "drop-client")

        event = _notice("x" * 2000)
        frames = build_frames(json.dumps(wire_dump(event)), "notice")
        oversized = [f for f in frames if f.size > hard]
        deliverable = [f for f in frames if f.size <= hard]
        assert oversized and deliverable, "本用例要求混合：部分帧超限、部分不超"

        with (
            patch.object(server, "_send_text", new_callable=AsyncMock) as spy,
            patch("wing.gateway.server.log") as mock_log,
        ):
            server._on_event(event)
            await asyncio.sleep(0.05)

        assert spy.await_count == len(deliverable), "不超限的帧必须照常投递"
        assert mock_log.warning.call_count == len(oversized), "每帧丢弃恰一行 WARN"
        messages = [c.args[0] for c in mock_log.warning.call_args_list]
        for message in messages:
            assert "drop-client" in message, message
            assert "notice" in message, message
            assert str(hard) in message, message
        assert set(messages) == {
            f"Dropping oversized frame for client drop-client: "
            f"type=notice size={f.size} > {hard}"
            for f in oversized
        }, "WARN 必须含廉价上下文（谁 / 类型 / 多大 / 阈值）"

    async def test_frames_within_hard_limit_are_sent(self, monkeypatch):
        """对照：把硬上限设得足够大时同一事件的切分帧全部送达。"""
        monkeypatch.setattr("wing.gateway.frames.SOFT_LIMIT_BYTES", 300)
        server = _make_server()
        stub = _RecordingWs()
        _register(server, _as_ws(stub))

        server._on_event(_notice("x" * 2000))
        await asyncio.sleep(0.05)
        assert len(stub.sent) >= 2

    async def test_send_failure_stops_remaining_frames(self, monkeypatch):
        monkeypatch.setattr("wing.gateway.frames.SOFT_LIMIT_BYTES", 300)
        server = _make_server()
        stub = _RecordingWs(fail_after=1)
        ws = _as_ws(stub)
        _register(server, ws, "dead-client")
        event_bus.route_attach("dead-client", SESSION)

        server._on_event(_notice("x" * 2000))
        await asyncio.sleep(0.05)

        assert len(stub.sent) == 1, "第 1 帧失败后必须停止剩余帧"
        assert "dead-client" not in server.clients, "写失败必须回收客户端"

    @pytest.mark.timeout(10)
    async def test_write_timeout_stops_remaining_frames(self, monkeypatch):
        monkeypatch.setattr("wing.gateway.frames.SOFT_LIMIT_BYTES", 300)
        monkeypatch.setattr("wing.gateway.server.WRITE_TIMEOUT_SECONDS", 0.05)
        server = _make_server()
        ws = _as_ws(_HangingWs())
        _register(server, ws, "slow-client")
        event_bus.route_attach("slow-client", SESSION)

        server._on_event(_notice("x" * 2000))
        await asyncio.sleep(0.3)

        assert "slow-client" not in server.clients, "写超时必须回收客户端"
        assert "slow-client" not in event_bus.routing_table

    async def test_small_events_keep_the_single_frame_path(self):
        """小事件必须与既有实现逐字节一致（不包装、不切分）。"""
        server = _make_server()
        stub = _RecordingWs()
        _register(server, _as_ws(stub))

        event = _notice("hello")
        server._on_event(event)
        await asyncio.sleep(0.05)

        assert stub.sent == [json.dumps(wire_dump(event))]


class TestEagerChunking:
    def test_build_frames_is_called_once_per_event(self, monkeypatch):
        """切分在 eager 阶段：一次事件只切一次，多目标共享同一批帧。"""
        server = _make_server()
        for cid in ("a", "b", "c"):
            _register(server, _as_ws(_RecordingWs()), cid)

        calls = 0
        real = build_frames

        def counting(payload: str, of_type: str):
            nonlocal calls
            calls += 1
            return real(payload, of_type)

        monkeypatch.setattr("wing.gateway.server.build_frames", counting)
        server._on_event(_notice("hello"))
        assert calls == 1
