"""ReActLoop._call_llm 契约测试——块数组是 Message 组装的唯一依据。

LLMCaller 已并入 ReActLoop：chunk 流消费、流式事件发射、Message 组装是
loop 的内部职责（实际模型调用在 provider.generate）。provider 未产出权威
content_blocks（流未正常结束）→ 报错，截断轮次 MUST NOT 作为成功 turn 提交。
"""

from __future__ import annotations

import pytest

from wing.agent.event_sink import AgentEventSink
from wing.agent.react_loop import ReActLoop
from wing.schema import LLMResponse, TextBlock, ThinkingBlock, ToolUseBlock


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
