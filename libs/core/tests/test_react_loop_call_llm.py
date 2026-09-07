"""ReActLoop._call_llm 契约测试——块数组是 Message 组装的唯一依据。

LLMCaller 已并入 ReActLoop：chunk 流消费、流式事件发射、Message 组装是
loop 的内部职责（实际模型调用在 provider.generate）。provider 未产出权威
content_blocks（流未正常结束）→ 报错，截断轮次 MUST NOT 作为成功 turn 提交。
"""

from __future__ import annotations

import pytest

from wing.agent.event_sink import AgentEventSink
from wing.agent.react_loop import ReActLoop
from wing.event import LLMCallMetricsEvent
from wing.event_bus import event_bus
from wing.schema import LLMResponse, LLMUsage, TextBlock, ThinkingBlock, ToolUseBlock


@pytest.fixture(autouse=True)
def cleanup_event_bus():
    event_bus._subscribers.clear()
    event_bus._routing.clear()
    yield
    event_bus._subscribers.clear()
    event_bus._routing.clear()


def _make_loop() -> ReActLoop:
    return ReActLoop(
        tool_executor=None,  # ty: ignore[invalid-argument-type]  # _call_llm 不触碰
        sink=AgentEventSink(session_id="test"),
        context_manager=None,  # ty: ignore[invalid-argument-type]  # _call_llm 不触碰
        inbox=None,  # ty: ignore[invalid-argument-type]  # _call_llm 不触碰
        current_model=lambda: "test-model",
        current_provider=lambda: None,  # ty: ignore[invalid-argument-type]
        current_tools=lambda: [],
        stream=True,
    )


class _FakeProvider:
    def __init__(self, chunks: list[LLMResponse]) -> None:
        self._chunks = chunks

    async def generate(self, **kwargs):
        for chunk in self._chunks:
            yield chunk

    def create_accumulator(self):
        # duck-type 协议：_call_llm 只透传 + 取消时调 snapshot_blocks
        return None

    def snapshot_blocks(self, accumulator):
        return None


class TestCallLlmContract:
    @pytest.mark.asyncio
    async def test_assembles_message_from_blocks(self):
        """最终 chunk 的块数组 → assistant Message（扁平字段派生）。"""
        provider = _FakeProvider(
            [
                LLMResponse(reasoning_content="hmm"),
                LLMResponse(content="hello"),
                LLMResponse(
                    content_blocks=[
                        ThinkingBlock(thinking="hmm", signature="s1"),
                        TextBlock(text="hello"),
                        ToolUseBlock(id="t1", name="Bash", input={"cmd": "ls"}),
                    ]
                ),
            ]
        )
        msg = await _make_loop()._call_llm(provider, [], "m", None)  # ty: ignore[invalid-argument-type]
        assert msg.role == "assistant"
        assert msg.content == "hello"
        assert msg.reasoning_content == "hmm"
        assert msg.tool_calls is not None and msg.tool_calls[0].name == "Bash"

    @pytest.mark.asyncio
    async def test_missing_blocks_raises_no_flat_fallback(self):
        """provider 未产出块数组（流截断）→ 报错，无扁平兜底。"""
        provider = _FakeProvider([LLMResponse(content="partial")])
        with pytest.raises(RuntimeError, match="content_blocks"):
            await _make_loop()._call_llm(provider, [], "m", None)  # ty: ignore[invalid-argument-type]

    @pytest.mark.asyncio
    async def test_metrics_emitted_once_per_call(self):
        """llm_metrics 每次调用只发射一次（锁定 token 双计回归）。

        provider 契约：非零 usage 由带内 chunk 携带，最终块数组 chunk 只带
        零 token 元信息。_call_llm 对每个非零 usage chunk 发射一次——若最终
        chunk 再附着非零 usage，metrics 事件翻倍。
        """
        events: list = []
        event_bus.subscribe(events.append)

        provider = _FakeProvider(
            [
                LLMResponse(content="hi"),
                LLMResponse(usage=LLMUsage(prompt_tokens=10, completion_tokens=5)),
                LLMResponse(
                    content_blocks=[TextBlock(text="hi")],
                    usage=LLMUsage(model="m"),  # 零 token 元信息（provider 契约）
                ),
            ]
        )
        msg = await _make_loop()._call_llm(provider, [], "m", None)  # ty: ignore[invalid-argument-type]

        metrics = [e for e in events if isinstance(e, LLMCallMetricsEvent)]
        assert len(metrics) == 1
        assert metrics[0].prompt_tokens == 10
        assert metrics[0].completion_tokens == 5
        # Message.usage 取自带内非零 chunk，不受终块零元信息影响
        assert msg.usage is not None
        assert msg.usage.prompt_tokens == 10
