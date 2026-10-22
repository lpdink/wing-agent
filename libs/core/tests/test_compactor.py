"""Compactor 单测：字段与阈值、切割点计算、do_compact、以及 ContextManager 的接线。"""

from __future__ import annotations

from typing import AsyncIterator

import pytest

from _support import default_compactor, make_cm
from wing.context import Compactor
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
        options: object = None,
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
        options: object = None,
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

    def test_set_window_rebinds_both_thresholds(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        c.set_window(context_window_tokens=1_000, keep_recent_tokens=400)
        assert c.context_window_tokens == 1_000
        assert c.keep_recent_tokens == 400
        assert c.compact_window_tokens == 600

    def test_set_window_rejects_invalid_pair_without_mutation(self):
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        with pytest.raises(
            ValueError, match="context_window_tokens must > keep_recent_tokens"
        ):
            c.set_window(context_window_tokens=100, keep_recent_tokens=200)
        # 先校验后赋值：失败路径状态零变化。
        assert c.context_window_tokens == 100_000
        assert c.keep_recent_tokens == 20_000
        assert c.compact_window_tokens == 80_000

    def test_copy_is_independent(self):
        """副本各自持有阈值：改写副本不影响原件（每会话一份的隔离语义）。"""
        original = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        clone = original.copy()
        assert clone is not original
        assert (clone.context_window_tokens, clone.keep_recent_tokens) == (
            100_000,
            20_000,
        )
        clone.set_window(context_window_tokens=1_000, keep_recent_tokens=400)
        assert original.context_window_tokens == 100_000
        assert original.keep_recent_tokens == 20_000
        assert clone.compact_window_tokens == 600

    def test_no_model_attr(self):
        assert not hasattr(
            Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000), "model"
        )
        assert not hasattr(
            Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000),
            "model_provider",
        )


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

    @pytest.mark.asyncio
    async def test_appends_compact_prompt_after_full_history(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [
            Message(role="system", content="You are helpful."),
            Message(role="user", content="hello"),
            Message(role="assistant", content="hi"),
        ]
        prov, state = _capturing_provider()
        resp = await h.do_compact(full, DEFAULT_MODEL, prov)
        msgs = state["messages"]
        assert len(msgs) == len(full) + 1
        assert msgs[-1].role == "user"
        assert "Write a continuation summary" in (msgs[-1].content or "")
        assert "DO NOT call any tools" in (msgs[-1].content or "")
        assert resp.content == "[Compact] ok"
        assert msgs[0].content == "You are helpful."
        assert msgs[1].content == "hello"
        assert msgs[2].content == "hi"

    @pytest.mark.asyncio
    async def test_tool_calls_ignored_and_summary_extracted_from_surrounding_text(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov = _mock_provider(
            LLMResponse(
                content="prefix<summary>real summary</summary>suffix",
                tool_calls=[ToolCall(id="x", name="Bash", arguments={"x": "y"})],
            )
        )
        resp = await h.do_compact(full, DEFAULT_MODEL, prov)
        assert resp.content == "[Compact] real summary"

    @pytest.mark.asyncio
    async def test_reasoning_content_preserved(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov = _mock_provider(
            LLMResponse(content="<summary>x</summary>", reasoning_content="think...")
        )
        resp = await h.do_compact(full, DEFAULT_MODEL, prov)
        assert resp.reasoning_content == "think..."

    @pytest.mark.asyncio
    async def test_model_passed_through(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov, state = _capturing_provider()
        await h.do_compact(full, "my-main-model", prov)
        assert state["model"] == "my-main-model"

    @pytest.mark.asyncio
    async def test_tools_passed_through(self):
        """tools 参数被透传给 provider.generate。"""
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov, state = _capturing_provider()
        my_tools = ["Bash", "Read"]
        await h.do_compact(full, "m", prov, tools=my_tools)
        assert state["tools"] == my_tools

    @pytest.mark.asyncio
    async def test_tools_defaults_to_none(self):
        """不传 tools 时透传 None（与主 agent 调用不一致时不命中缓存，但兼容）。"""
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        full = [Message(role="system", content="You are helpful.")]
        prov, state = _capturing_provider()
        await h.do_compact(full, "m", prov)
        assert state["tools"] is None

    @pytest.mark.asyncio
    async def test_empty_full_messages(self):
        h = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        prov, state = _capturing_provider()
        await h.do_compact([], DEFAULT_MODEL, prov)
        assert len(state["messages"]) == 1
        assert state["messages"][0].role == "user"


# ===================================================================
# 5. ContextManager 接线（get_messages_for_llm 的压缩触发 / 应用路径）
# ===================================================================


class TestGetMessagesForLlm:
    @pytest.mark.asyncio
    async def test_accepts_model_and_provider(self, tmp_dir):
        cm = make_cm(tmp_dir, compactor=default_compactor())
        cm.add_message(Message(role="user", content="hello"))
        llm_msgs = (
            await cm.get_messages_for_llm(
                model=DEFAULT_MODEL,
                model_provider=_mock_provider(
                    LLMResponse(content="<summary>t</summary>")
                ),
                current_tools=lambda: [],
            )
        ).messages
        assert len(llm_msgs) == 2
        assert llm_msgs[1].content == "hello"

    @pytest.mark.asyncio
    async def test_no_compact_path(self, tmp_dir):
        cm = make_cm(tmp_dir, compactor=default_compactor())
        cm.add_message(Message(role="user", content="hello"))
        cm.add_message(Message(role="assistant", content="hi"))
        llm_msgs = (
            await cm.get_messages_for_llm(
                model=DEFAULT_MODEL,
                model_provider=_mock_provider(
                    LLMResponse(content="<summary>t</summary>")
                ),
                current_tools=lambda: [],
            )
        ).messages
        assert len(llm_msgs) == 3

    @pytest.mark.asyncio
    async def test_compact_uses_main_model(self, tmp_dir):
        """Background compact passes the main model to the provider."""
        low_threshold = Compactor(context_window_tokens=50, keep_recent_tokens=10)
        cm = make_cm(tmp_dir, compactor=low_threshold)
        cm.add_message(Message(role="user", content="x" * 200))
        cm.add_message(Message(role="assistant", content="y" * 200))
        cm.add_message(Message(role="user", content="z" * 50))
        prov, state = _capturing_provider()
        await cm.get_messages_for_llm(
            model="main-model", model_provider=prov, current_tools=lambda: []
        )
        if cm._pending_compact_task:
            await cm._pending_compact_task
        assert state.get("model") == "main-model"

    @pytest.mark.asyncio
    async def test_compact_failure_no_result(self, tmp_dir):
        """Background compact failure → result stays None, original messages returned."""
        low_threshold = Compactor(context_window_tokens=50, keep_recent_tokens=10)
        cm = make_cm(tmp_dir, compactor=low_threshold)
        cm.add_messages(
            [
                Message(role="user", content="x" * 500),
                Message(role="assistant", content="y" * 500),
                Message(role="user", content="z" * 500),
            ]
        )
        prov = _mock_provider(LLMResponse(content="no summary tags"))
        llm_msgs = (
            await cm.get_messages_for_llm(
                model="m", model_provider=prov, current_tools=lambda: []
            )
        ).messages
        if cm._pending_compact_task:
            await cm._pending_compact_task
        assert cm._pending_compact_result is None
        assert len(llm_msgs) == 4  # system + 3 original messages


class TestCompactIntegration:
    @pytest.mark.asyncio
    async def test_compact_then_continue(self, tmp_dir):
        """Full async compact flow: early trigger → background → apply → continue."""
        low_threshold = Compactor(context_window_tokens=50, keep_recent_tokens=10)
        cm = make_cm(tmp_dir, compactor=low_threshold)
        cm.add_messages(
            [
                Message(role="user", content="old1"),
                Message(role="assistant", content="old2 is a long message " * 8),
                Message(role="user", content="old3"),
            ]
        )

        async def _gen(
            self,
            messages: list[Message],
            model: str,
            tools: list | None = None,
            stream: bool = False,
            options: object = None,
        ) -> AsyncIterator[LLMResponse]:
            yield LLMResponse(content="<summary>compressed old1 and old2</summary>")

        prov = type("MockProvider", (), {"generate": _gen})()

        await cm.get_messages_for_llm(
            model="main-model",
            model_provider=prov,  # ty: ignore[invalid-argument-type]
            current_tools=lambda: [],
        )

        # Wait for background task
        if cm._pending_compact_task:
            await cm._pending_compact_task

        cm.add_message(Message(role="assistant", content="resp " * 20))
        cm.add_message(Message(role="user", content="more " * 20))

        llm_msgs = (
            await cm.get_messages_for_llm(
                model="main-model",
                model_provider=prov,  # ty: ignore[invalid-argument-type]
                current_tools=lambda: [],
            )
        ).messages

        assert len(llm_msgs) >= 2
        compact_found = any("[Compact]" in (m.content or "") for m in llm_msgs)
        if not compact_found:
            assert cm._pending_compact_result is not None  # still pending

        cm.add_message(Message(role="user", content="new question"))
        window = cm.get_context_window()
        assert len(window) >= 2
        assert "new question" in (window[-1].content or "")
