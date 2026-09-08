"""Tests for Compactor — 重命名后的字段、阈值判断、切割点计算。"""

from __future__ import annotations

from typing import AsyncIterator

import pytest

from wing.compactor import Compactor
from wing.schema import LLMResponse, Message, ToolCall


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

DEFAULT_MODEL = "test-model"


def _capturing_provider():
    """Return (provider, state_dict) -- state_dict is populated on call."""
    state: dict = {}

    async def _gen(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        state["messages"] = messages
        state["model"] = model
        state["tools"] = tools
        yield LLMResponse(content="<summary>ok</summary>")

    return type("MockProvider", (), {"generate": _gen})(), state


def _mock_provider(response: LLMResponse):
    async def _gen(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        yield response

    return type("MockProvider", (), {"generate": _gen})()


# ===================================================================
# 1. Init & Renamed Fields
# ===================================================================


class TestCompactorInit:
    def test_init_with_values(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        assert c.context_window_tokens == 100_000
        assert c.keep_recent_tokens == 20_000
        assert c.compact_window_tokens == 80_000

    def test_requires_both_params(self):
        with pytest.raises(TypeError):
            Compactor()  # ty: ignore[missing-argument]

    def test_custom_values(self):
        c = Compactor(context_window_tokens=256_000, keep_recent_tokens=50_000)
        assert c.context_window_tokens == 256_000
        assert c.keep_recent_tokens == 50_000
        assert c.compact_window_tokens == 206_000

    def test_constraint_violation(self):
        with pytest.raises(
            ValueError, match="context_window_tokens must > keep_recent_tokens"
        ):
            Compactor(context_window_tokens=100, keep_recent_tokens=200)

    def test_no_old_attrs(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        assert not hasattr(c, "trigger_compact_tokens")
        assert not hasattr(c, "compact_target_tokens")


# ===================================================================
# 2. Threshold Methods
# ===================================================================


class TestThresholdMethods:
    def test_need_early_trigger_below(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        # compact_window_tokens = 80_000
        msgs = [Message(role="user", content="x" * 100)]  # ~25 tokens
        assert c.need_early_trigger(msgs) is False

    def test_need_early_trigger_above(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [Message(role="user", content="x" * 400_000)]  # ~100K tokens
        assert c.need_early_trigger(msgs) is True

    def test_need_early_trigger_server_tokens(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [Message(role="user", content="short")]
        # server_tokens overrides local estimation
        assert c.need_early_trigger(msgs, server_tokens=80_000) is True
        assert c.need_early_trigger(msgs, server_tokens=79_999) is False

    def test_need_apply_compact_below(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [Message(role="user", content="x" * 100)]
        assert c.need_apply_compact(msgs) is False

    def test_need_apply_compact_above(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [Message(role="user", content="x" * 500_000)]  # ~125K tokens
        assert c.need_apply_compact(msgs) is True

    def test_need_apply_compact_server_tokens(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [Message(role="user", content="short")]
        assert c.need_apply_compact(msgs, server_tokens=100_000) is True
        assert c.need_apply_compact(msgs, server_tokens=99_999) is False


# ===================================================================
# 3. _calc_cut_idx
# ===================================================================


class TestCalcCutIdx:
    def test_basic_cut(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=80_000)
        # compact_window_tokens = 20_000
        msgs = [
            Message(role="user", content="short"),
            Message(role="assistant", content="x" * 80_000),  # ~20K tokens
            Message(role="user", content="tail"),
        ]
        idx = c._calc_cut_idx(msgs)
        assert idx == 2  # first two messages compacted

    def test_cut_all_if_small(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [Message(role="user", content="tiny") for _ in range(10)]
        idx = c._calc_cut_idx(msgs)
        assert idx == len(msgs)

    def test_tool_call_integrity(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=99_000)
        msgs = [
            Message(role="user", content="x" * 100),
            Message(
                role="assistant",
                content="",
                tool_calls=[ToolCall(id="tc1", name="Bash", arguments={"x": "y"})],
            ),
            Message(role="tool", content="result", tool_call_id="tc1"),
            Message(role="user", content="tail"),
        ]
        idx = c._calc_cut_idx(msgs)
        assert idx == len(msgs)

    def test_tool_call_integrity_with_pending(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        msgs = [
            Message(role="user", content="short"),
            Message(
                role="assistant",
                content="",
                tool_calls=[ToolCall(id="tc1", name="Bash", arguments={"x": "y"})],
            ),
            Message(role="tool", content="result", tool_call_id="tc1"),
            Message(role="user", content="tail"),
        ]
        idx = c._calc_cut_idx(msgs)
        assert idx >= 3

    def test_empty_list(self):
        assert (
            Compactor(
                context_window_tokens=100_000, keep_recent_tokens=20_000
            )._calc_cut_idx([])
            == 0
        )


# ===================================================================
# 4. do_compact
# ===================================================================


class TestDoCompact:
    @pytest.mark.asyncio
    async def test_appends_compact_prompt(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [
            Message(role="system", content="You are helpful."),
            Message(role="user", content="hello"),
        ]
        prov, state = _capturing_provider()
        resp = await c.do_compact(full, DEFAULT_MODEL, prov)
        msgs = state["messages"]
        assert len(msgs) == len(full) + 1
        assert msgs[-1].role == "user"
        assert "Write a continuation summary" in (msgs[-1].content or "")
        assert resp.content == "[Compact] ok"

    @pytest.mark.asyncio
    async def test_instruction_inserted_between_body_and_format(self):
        """用户指令条件插入 BODY 与 FORMAT 之间：CRITICAL 保持末位 recency。"""
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="sys")]
        prov, state = _capturing_provider()

        await c.do_compact(
            full, DEFAULT_MODEL, prov, instruction="保留架构决策与未完成的 TODO"
        )
        content = state["messages"][-1].content or ""
        assert "Additional instruction from the user" in content
        assert "保留架构决策与未完成的 TODO" in content
        # 顺序：主体末段 → 用户指令 → <summary> 标签约束 → CRITICAL
        body_end = content.index("immediate resumption of the task.")
        ins = content.index("保留架构决策与未完成的 TODO")
        wrap = content.index("Wrap your summary in <summary></summary> tags.")
        critical = content.index("## CRITICAL")
        assert body_end < ins < wrap < critical

    @pytest.mark.asyncio
    async def test_no_instruction_prompt_byte_identical(self):
        """无指令 → prompt 与 COMPACT_PROMPT 逐字节一致。
        自动压缩与裸手动压缩（高频、经过大量验证的路径）零影响。"""
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="sys")]
        prov, state = _capturing_provider()

        await c.do_compact(full, DEFAULT_MODEL, prov)
        assert state["messages"][-1].content == c.COMPACT_PROMPT

    def test_compact_prompt_split_identity(self):
        """拆分恒等式：BODY + FORMAT == COMPACT_PROMPT（原始提示词未被改动，
        只做了条件性增加）。防止后续直接改动其一，导致带指令/不带指令
        两条路径的 prompt 漂移。"""
        assert (
            Compactor._COMPACT_PROMPT_BODY + Compactor._COMPACT_PROMPT_FORMAT
            == Compactor.COMPACT_PROMPT
        )

    @pytest.mark.asyncio
    async def test_blank_instruction_ignored(self):
        """空白指令视同未提供——prompt 与默认逐字节一致。"""
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="sys")]
        prov, state = _capturing_provider()

        await c.do_compact(full, DEFAULT_MODEL, prov, instruction="   ")
        content = state["messages"][-1].content or ""
        assert content == c.COMPACT_PROMPT

    @pytest.mark.asyncio
    async def test_tool_calls_silently_ignored(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="sys")]
        prov = _mock_provider(
            LLMResponse(
                content="<summary>real</summary>",
                tool_calls=[ToolCall(id="x", name="Bash", arguments={"x": "y"})],
            )
        )
        resp = await c.do_compact(full, DEFAULT_MODEL, prov)
        assert resp.content == "[Compact] real"

    @pytest.mark.asyncio
    async def test_missing_summary_raises(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="sys")]
        prov = _mock_provider(LLMResponse(content="no tags"))
        with pytest.raises(RuntimeError, match="extract summary"):
            await c.do_compact(full, DEFAULT_MODEL, prov)

    @pytest.mark.asyncio
    async def test_empty_messages(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        prov, state = _capturing_provider()
        await c.do_compact([], DEFAULT_MODEL, prov)
        assert len(state["messages"]) == 1  # only COMPACT_PROMPT
