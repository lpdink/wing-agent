"""剧本 → SSE 字节流对账（tasks 3.6）。

独立解析器（不 import wing 的 SSE 实现）把字节流解析回结构，逐字段核对：
帧序、``delta`` 字段名、tool_calls 增量与切断、``finish_reason``、usage 末帧、
``data: [DONE]`` 终止；另覆盖非流式响应与剧本路由（未注册 / 耗尽）。
"""

import contextlib
import json
from collections.abc import AsyncIterator

import httpx
import pytest

from wing_probe.provider.script import (
    Script,
    ScriptExhaustedError,
    ScriptRegistry,
    ToolCall,
    Turn,
    UnregisteredModelError,
    Usage,
    resolve_call_id,
    split_text,
)
from wing_probe.provider.server import FakeProvider
from wing_probe.provider.sse import (
    SSE_CONTENT_TYPE,
    completion_response,
    encode_turn_stream,
    stream_frames,
)

MODEL = "probe/unit"
COMPLETION_ID = "chatcmpl-probe-0"
CREATED = 1_700_000_000


# ── 独立解析器（外来消费者视角） ────────────────────────────────


def parse_sse(payload: bytes) -> list[dict | None]:
    """按 SSE 规范解析帧：``data: {json}\\n\\n``；``[DONE]`` → None。"""
    text = payload.decode("utf-8")
    assert text.endswith("\n\n"), repr(text[-20:])
    frames: list[dict | None] = []
    for raw in text.split("\n\n"):
        if not raw:
            continue
        assert raw.startswith("data: "), repr(raw)
        data = raw[len("data: ") :]
        frames.append(None if data == "[DONE]" else json.loads(data))
    return frames


def deltas(frames: list[dict | None]) -> list[dict]:
    out = []
    for frame in frames:
        if frame is None:
            continue
        choices = frame["choices"]
        if not choices:
            continue
        out.append(choices[0]["delta"])
    return out


def content_of(frames: list[dict | None]) -> str:
    return "".join(d.get("content", "") for d in deltas(frames))


def reasoning_of(frames: list[dict | None]) -> str:
    return "".join(d.get("reasoning_content", "") for d in deltas(frames))


def tool_fragments(frames: list[dict | None]) -> list[dict]:
    out = []
    for delta in deltas(frames):
        for call in delta.get("tool_calls", []):
            out.append(call)
    return out


def encode(turn: Turn, *, turn_index: int = 0) -> list[dict | None]:
    payload = encode_turn_stream(
        turn,
        turn_index=turn_index,
        model=MODEL,
        completion_id=COMPLETION_ID,
        created=CREATED,
    )
    return parse_sse(payload)


# ── 帧形 ────────────────────────────────────────────────────────


def test_text_turn_frame_sequence() -> None:
    turn = Turn.of(
        text="hello world", usage=Usage(prompt_tokens=11, completion_tokens=2)
    )
    frames = encode(turn)

    assert frames[-1] is None, "last frame must be [DONE]"
    assert content_of(frames) == "hello world"

    # 首帧：role 空帧
    first = frames[0]
    assert first is not None
    assert first["object"] == "chat.completion.chunk"
    assert first["id"] == COMPLETION_ID
    assert first["model"] == MODEL
    assert first["created"] == CREATED
    assert first["choices"][0]["delta"] == {"role": "assistant", "content": ""}

    # finish 帧
    finish = frames[-2 - 1]
    assert finish is not None
    assert finish["choices"][0]["finish_reason"] == "stop"
    assert finish["choices"][0]["delta"] == {}

    # 末帧（DONE 前）：usage 帧，choices 为空、usage 带 cached 明细
    usage_frame = frames[-2]
    assert usage_frame is not None
    assert usage_frame["choices"] == []
    assert usage_frame["usage"] == {
        "prompt_tokens": 11,
        "completion_tokens": 2,
        "total_tokens": 13,
        "prompt_tokens_details": {"cached_tokens": 0},
    }


def test_default_usage_is_zeroed() -> None:
    frames = encode(Turn.of(text="x"))
    usage_frame = frames[-2]
    assert usage_frame is not None
    assert usage_frame["usage"]["prompt_tokens"] == 0
    assert usage_frame["usage"]["completion_tokens"] == 0


def test_text_chunking_granularity() -> None:
    frames = encode(Turn.of(text="abcdefgh", chunk=3))
    assert content_of(frames) == "abcdefgh"
    assert [
        d["content"] for d in deltas(frames) if "content" in d and d["content"]
    ] == [
        "abc",
        "def",
        "gh",
    ]


def test_thinking_precedes_text_and_is_chunked() -> None:
    frames = encode(Turn.of(thinking="思考一下", text="答案", chunk=2))
    assert reasoning_of(frames) == "思考一下"
    assert content_of(frames) == "答案"
    keys = [next(iter(d)) for d in deltas(frames) if d and next(iter(d), "") != "role"]
    assert keys == ["reasoning_content", "reasoning_content", "content"]


def test_tool_call_single_argument_frame() -> None:
    turn = Turn.of(tool_calls=[ToolCall("Bash", {"command": "ls"})])
    frames = encode(turn)

    fragments = tool_fragments(frames)
    assert len(fragments) == 1
    assert fragments[0]["index"] == 0
    assert fragments[0]["id"] == "call_0_0"
    assert fragments[0]["type"] == "function"
    assert fragments[0]["function"]["name"] == "Bash"
    assert fragments[0]["function"]["arguments"] == '{"command": "ls"}'
    assert json.loads(fragments[0]["function"]["arguments"]) == {"command": "ls"}

    finish = [f for f in frames if f and f["choices"]][-1]
    assert finish["choices"][0]["finish_reason"] == "tool_calls"


def test_tool_call_arguments_cut_mid_json() -> None:
    call = ToolCall("Bash", {"command": "ls -la"}, cut=5)
    frames = encode(Turn.of(tool_calls=[call]))
    fragments = tool_fragments(frames)

    assert len(fragments) == 2
    # 首片带 id/name，第二片只有 index + arguments 增量
    assert fragments[0]["function"]["name"] == "Bash"
    assert fragments[0]["function"]["arguments"] == '{"com'
    assert "name" not in fragments[1]["function"]
    assert "id" not in fragments[1]
    assert fragments[1]["index"] == 0
    assert fragments[1]["function"]["arguments"] == 'mand": "ls -la"}'
    assert fragments[0]["function"]["arguments"] + fragments[1]["function"][
        "arguments"
    ] == json.dumps({"command": "ls -la"})


def test_tool_call_multiple_cuts_and_indices() -> None:
    turn = Turn.of(
        tool_calls=[
            ToolCall("Bash", '{"a": 1}', cut=[2, 4]),
            ToolCall("Read", {"path": "/tmp/x"}),
        ]
    )
    frames = encode(turn)
    fragments = tool_fragments(frames)
    assert [f["index"] for f in fragments] == [0, 0, 0, 1]
    assert "".join(f["function"]["arguments"] for f in fragments[:3]) == '{"a": 1}'
    assert [f.get("id") for f in fragments] == [
        "call_0_0",
        None,
        None,
        "call_0_1",
    ]


def test_cut_validation() -> None:
    with pytest.raises(ValueError, match="strictly increasing"):
        ToolCall("Bash", '{"a": 1}', cut=[4, 2])
    with pytest.raises(ValueError, match="within 1"):
        ToolCall("Bash", '{"a": 1}', cut=99)
    with pytest.raises(ValueError, match="must not be empty"):
        ToolCall("", {})
    with pytest.raises(ValueError, match="chunk"):
        Turn.of(text="x", chunk=0)
    with pytest.raises(ValueError, match="delay"):
        Turn.of(text="x", delay=-1.0)


def test_frames_carry_delay_between_chunks() -> None:
    turn = Turn.of(text="abcd", chunk=2, delay=0.25)
    frames = stream_frames(
        turn,
        turn_index=0,
        model=MODEL,
        completion_id=COMPLETION_ID,
        created=CREATED,
    )
    assert frames[0].delay == 0.0, "首帧不等待（尽快交出响应头）"
    assert all(frame.delay == 0.25 for frame in frames[1:])


def test_explicit_call_id_wins() -> None:
    call = ToolCall("Bash", {"command": "ls"}, id="call_fixed")
    assert resolve_call_id(call, turn_index=3, call_index=7) == "call_fixed"
    assert (
        resolve_call_id(ToolCall("Bash", {}), turn_index=3, call_index=7) == "call_3_7"
    )


def test_split_text() -> None:
    assert split_text(None, 2) == []
    assert split_text("", 2) == []
    assert split_text("abc", None) == ["abc"]
    assert split_text("abc", 5) == ["abc"]
    assert split_text("abcde", 2) == ["ab", "cd", "e"]


def test_sse_frame_text_shape() -> None:
    frames = stream_frames(
        Turn.of(text="hi"),
        turn_index=0,
        model=MODEL,
        completion_id=COMPLETION_ID,
        created=CREATED,
    )
    assert frames[0].text.startswith("data: {")
    assert frames[0].text.endswith("\n\n")
    assert frames[-1].text == "data: [DONE]\n\n"
    assert frames[0].encode() == frames[0].text.encode("utf-8")
    assert SSE_CONTENT_TYPE.startswith("text/event-stream")


def test_non_streaming_completion_response_ignores_cut() -> None:
    turn = Turn.of(
        thinking="内省",
        text="完成",
        tool_calls=[ToolCall("Bash", {"command": "ls"}, cut=3)],
        usage=Usage(prompt_tokens=5, completion_tokens=7, cached_tokens=2),
        finish="stop",
    )
    payload = completion_response(
        turn,
        turn_index=1,
        model=MODEL,
        completion_id=COMPLETION_ID,
        created=CREATED,
    )
    assert payload["object"] == "chat.completion"
    choice = payload["choices"][0]
    assert choice["finish_reason"] == "stop"
    message = choice["message"]
    assert message["role"] == "assistant"
    assert message["content"] == "完成"
    assert message["reasoning_content"] == "内省"
    assert message["tool_calls"] == [
        {
            "id": "call_1_0",
            "type": "function",
            "function": {"name": "Bash", "arguments": json.dumps({"command": "ls"})},
        }
    ]
    assert payload["usage"] == {
        "prompt_tokens": 5,
        "completion_tokens": 7,
        "total_tokens": 12,
        "prompt_tokens_details": {"cached_tokens": 2},
    }


def test_non_streaming_without_text_keeps_empty_content() -> None:
    payload = completion_response(
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "ls"})]),
        turn_index=0,
        model=MODEL,
        completion_id=COMPLETION_ID,
        created=CREATED,
    )
    assert payload["choices"][0]["message"]["content"] == ""
    assert payload["choices"][0]["finish_reason"] == "tool_calls"


# ── 剧本路由 ────────────────────────────────────────────────────


def test_script_consumes_turns_in_order() -> None:
    script = Script(Turn.of(text="one"), Turn.of(text="two"))
    assert len(script) == 2
    assert script.remaining == 2
    first, index = script.consume(model=MODEL)
    assert (first.text, index) == ("one", 0)
    second, index = script.consume(model=MODEL)
    assert (second.text, index) == ("two", 1)
    assert script.consumed == 2 and script.remaining == 0
    with pytest.raises(ScriptExhaustedError) as excinfo:
        script.consume(model=MODEL)
    assert "probe/unit" in str(excinfo.value)
    assert "consumed 2/2" in str(excinfo.value)
    assert excinfo.value.code == "script_exhausted"


def test_script_reset_replays() -> None:
    script = Script(Turn.of(text="one"))
    script.consume(model=MODEL)
    script.reset()
    assert script.consumed == 0
    turn, index = script.consume(model=MODEL)
    assert (turn.text, index) == ("one", 0)


def test_script_requires_turns() -> None:
    with pytest.raises(ValueError, match="at least one Turn"):
        Script()


def test_registry_routes_by_model_and_reports_misses() -> None:
    registry = ScriptRegistry()
    registry.register("probe/a", Script(Turn.of(text="a1"), Turn.of(text="a2")))
    registry.register("probe/b", Script(Turn.of(text="b1")))
    assert registry.models() == ["probe/a", "probe/b"]
    assert "probe/a" in registry and len(registry) == 2

    turn, index = registry.consume("probe/a")
    assert (turn.text, index) == ("a1", 0)
    assert registry.get("probe/a").consumed == 1
    assert registry.get("probe/b").consumed == 0

    registry.consume("probe/a")
    with pytest.raises(ScriptExhaustedError):
        registry.consume("probe/a")

    with pytest.raises(UnregisteredModelError) as excinfo:
        registry.consume("probe/missing")
    assert "probe/missing" in str(excinfo.value)
    assert "probe/a" in str(excinfo.value), "报告应列出已注册模型"
    assert excinfo.value.code == "unregistered_model"

    registry.clear()
    assert registry.models() == []
    with pytest.raises(UnregisteredModelError, match="<none>"):
        registry.consume("probe/a")


def test_registry_rejects_duplicate_registration() -> None:
    registry = ScriptRegistry()
    registry.register("probe/a", Script(Turn.of(text="a")))
    with pytest.raises(ValueError, match="already has a script"):
        registry.register("probe/a", Script(Turn.of(text="b")))
    with pytest.raises(ValueError, match="must not be empty"):
        registry.register("", Script(Turn.of(text="a")))


def test_turn_describe_mentions_finish_and_usage() -> None:
    turn = Turn.of(text="hi", usage=Usage(prompt_tokens=1, completion_tokens=2))
    described = turn.describe()
    assert "text='hi'" in described
    assert "finish=stop" in described
    assert "1in/2out" in described


# ── HTTP 面（进程内假 Provider，无子进程） ──────────────────────


@contextlib.asynccontextmanager
async def running_provider() -> AsyncIterator[FakeProvider]:
    provider = FakeProvider()
    await provider.start()
    try:
        yield provider
    finally:
        await provider.stop()


def request_body(**overrides: object) -> dict:
    body: dict = {
        "model": MODEL,
        "stream": True,
        "messages": [{"role": "user", "content": "hi"}],
    }
    body.update(overrides)
    return body


@pytest.mark.asyncio
async def test_server_streams_script_over_http() -> None:
    async with running_provider() as provider:
        provider.register(
            MODEL,
            Script(
                Turn.of(
                    thinking="t",
                    text="hi there",
                    usage=Usage(prompt_tokens=3, completion_tokens=4),
                    chunk=4,
                )
            ),
        )
        assert provider.port > 0, "port=0 由 OS 分配后读回真实端口"
        async with httpx.AsyncClient(
            base_url=provider.base_url, timeout=10.0
        ) as client:
            response = await client.post("/chat/completions", json=request_body())
            assert response.status_code == 200
            assert response.headers["content-type"].startswith("text/event-stream")
            frames = parse_sse(response.content)
            assert frames[-1] is None
            assert content_of(frames) == "hi there"
            assert reasoning_of(frames) == "t"
            usage_frame = frames[-2]
            assert usage_frame is not None
            assert usage_frame["usage"]["prompt_tokens"] == 3

        assert provider.requests.summary() == f"1 request(s): {MODEL}×1"
        logged = provider.requests.get(MODEL, 0)
        assert logged.stream is True
        assert logged.body["messages"][0]["content"] == "hi"
        assert logged.context().role_sequence() == ["user"]


@pytest.mark.asyncio
async def test_server_supports_non_streaming_compaction_call() -> None:
    async with running_provider() as provider:
        provider.register(MODEL, Script(Turn.of(text="<summary>done</summary>")))
        async with httpx.AsyncClient(
            base_url=provider.base_url, timeout=10.0
        ) as client:
            response = await client.post(
                "/chat/completions", json=request_body(stream=False)
            )
            assert response.status_code == 200
            payload = response.json()
            assert payload["object"] == "chat.completion"
            assert payload["choices"][0]["message"]["content"] == (
                "<summary>done</summary>"
            )
        assert provider.requests.get(MODEL).stream is False


@pytest.mark.asyncio
async def test_server_error_report_for_unknown_and_exhausted_scripts() -> None:
    async with running_provider() as provider:
        provider.register(MODEL, Script(Turn.of(text="only one")))
        async with httpx.AsyncClient(
            base_url=provider.base_url, timeout=10.0
        ) as client:
            ok = await client.post("/chat/completions", json=request_body())
            assert ok.status_code == 200

            exhausted = await client.post("/chat/completions", json=request_body())
            assert exhausted.status_code == 500
            report = exhausted.json()["error"]
            assert report["code"] == "script_exhausted"
            assert "consumed 1/1" in report["message"]

            unknown = await client.post(
                "/chat/completions", json=request_body(model="probe/nope")
            )
            assert unknown.status_code == 500
            report = unknown.json()["error"]
            assert report["code"] == "unregistered_model"
            assert "probe/nope" in report["message"]
            assert MODEL in report["message"], "报告列出已注册模型"

            models = await client.get("/models")
            assert models.status_code == 200
            assert [entry["id"] for entry in models.json()["data"]] == [MODEL]

            bad = await client.post(
                "/chat/completions",
                content=b"not json",
                headers={"Content-Type": "application/json"},
            )
            assert bad.status_code == 400

        # 未注册 / 耗尽的请求同样留档（诊断不丢现场）
        assert provider.requests.count() == 3
        assert provider.requests.count(MODEL) == 2
        assert [entry.model for entry in provider.requests] == [
            MODEL,
            MODEL,
            "probe/nope",
        ]


@pytest.mark.asyncio
async def test_provider_stop_is_idempotent_and_restartable() -> None:
    provider = FakeProvider()
    await provider.start()
    assert provider.started is True
    assert provider.port > 0
    await provider.stop()
    assert provider.started is False
    await provider.stop()  # 幂等
    await provider.start()
    try:
        assert provider.started is True
        assert provider.port > 0
    finally:
        await provider.stop()
