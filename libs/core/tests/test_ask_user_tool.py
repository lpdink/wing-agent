"""Tests for the AskUserQuestion tool: schema normalization, answer
pass-through, and cancel handling."""

import asyncio
from typing import Any

import pytest

from wing.event import AskEvent
from wing.schema import ToolError
from wing.tools.ask_user import (
    ASK_CANCEL_TOKEN,
    CANCEL_RESULT,
    ask_user,
)


class FakeCtx:
    """Minimal ToolContext stub capturing the emitted AskEvent."""

    def __init__(self, feedback: Any = "") -> None:
        self.session_id = "test-session"
        self._feedback = feedback
        self.event: AskEvent | None = None
        self.timeout: float | None = None

    async def ask_feedback(self, event: AskEvent, timeout: float) -> str:
        self.event = event
        self.timeout = timeout
        if isinstance(self._feedback, BaseException):
            raise self._feedback
        return self._feedback


@pytest.mark.asyncio
async def test_new_schema_normalized_into_event():
    ctx = FakeCtx(feedback="配色方案: 深色主题")
    result = await ask_user(
        [
            {
                "id": "theme",
                "header": "配色方案",
                "question": "更喜欢哪种配色？",
                "multiSelect": False,
                "options": [
                    {"label": "浅色主题", "description": "适合白天"},
                    {"label": "深色主题"},
                ],
            }
        ],
        ctx,
    )
    assert result == "配色方案: 深色主题"
    assert ctx.event is not None
    assert ctx.timeout == 6000
    (q,) = ctx.event.questions
    assert q["id"] == "theme"
    assert q["header"] == "配色方案"
    assert q["multiSelect"] is False
    assert q["options"] == [
        {"label": "浅色主题", "description": "适合白天"},
        {"label": "深色主题", "description": ""},
    ]


@pytest.mark.asyncio
async def test_multi_select_flag_serialized_camel_case():
    ctx = FakeCtx()
    await ask_user(
        [
            {
                "id": "features",
                "header": "测试项",
                "question": "测哪些？",
                "multiSelect": True,
                "options": [{"label": "A"}, {"label": "B"}],
            }
        ],
        ctx,
    )
    assert ctx.event is not None
    assert ctx.event.questions[0]["multiSelect"] is True


@pytest.mark.asyncio
async def test_legacy_choices_coerced_to_options():
    ctx = FakeCtx()
    await ask_user(
        [
            {
                "id": "q1",
                "question": "continue?",
                "choices": ["y", "n"],
            }
        ],
        ctx,
    )
    assert ctx.event is not None
    (q,) = ctx.event.questions
    assert q["header"] == "q1"  # missing header falls back to id
    assert q["options"] == [
        {"label": "y", "description": ""},
        {"label": "n", "description": ""},
    ]


@pytest.mark.asyncio
async def test_cancel_sentinel_translated():
    ctx = FakeCtx(feedback=ASK_CANCEL_TOKEN)
    result = await ask_user(
        [{"id": "q1", "header": "Q", "question": "proceed?"}],
        ctx,
    )
    assert result == CANCEL_RESULT


@pytest.mark.asyncio
async def test_free_form_question_without_options():
    ctx = FakeCtx(feedback="随便聊聊")
    result = await ask_user(
        [{"id": "q1", "header": "聊天", "question": "说点什么？"}],
        ctx,
    )
    assert result == "随便聊聊"
    assert ctx.event is not None
    assert ctx.event.questions[0]["options"] == []


@pytest.mark.asyncio
async def test_timeout_raises_tool_error():
    ctx = FakeCtx(feedback=asyncio.TimeoutError())
    with pytest.raises(ToolError) as exc:
        await ask_user(
            [{"id": "q1", "header": "Q", "question": "proceed?"}],
            ctx,
        )
    assert "timeout" in str(exc.value).lower()


@pytest.mark.asyncio
async def test_empty_questions_rejected():
    with pytest.raises(ToolError):
        await ask_user([], FakeCtx())


@pytest.mark.asyncio
async def test_duplicate_ids_rejected():
    with pytest.raises(ToolError) as exc:
        await ask_user(
            [
                {"id": "q1", "header": "A", "question": "one?"},
                {"id": "q1", "header": "B", "question": "two?"},
            ],
            FakeCtx(),
        )
    assert "Duplicate question id" in str(exc.value)


@pytest.mark.asyncio
async def test_empty_question_text_rejected():
    with pytest.raises(ToolError):
        await ask_user([{"id": "q1", "header": "A", "question": "   "}], FakeCtx())


@pytest.mark.asyncio
async def test_empty_option_label_rejected():
    with pytest.raises(ToolError):
        await ask_user(
            [
                {
                    "id": "q1",
                    "header": "A",
                    "question": "one?",
                    "options": [{"label": "  "}],
                }
            ],
            FakeCtx(),
        )


@pytest.mark.asyncio
async def test_duplicate_option_labels_rejected():
    with pytest.raises(ToolError):
        await ask_user(
            [
                {
                    "id": "q1",
                    "header": "A",
                    "question": "one?",
                    "options": [{"label": "x"}, {"label": "x"}],
                }
            ],
            FakeCtx(),
        )


@pytest.mark.asyncio
async def test_too_many_options_rejected():
    with pytest.raises(ToolError):
        await ask_user(
            [
                {
                    "id": "q1",
                    "header": "A",
                    "question": "one?",
                    "options": [{"label": str(i)} for i in range(5)],
                }
            ],
            FakeCtx(),
        )


@pytest.mark.asyncio
async def test_too_many_questions_rejected():
    with pytest.raises(ToolError):
        await ask_user(
            [{"id": f"q{i}", "header": "A", "question": "one?"} for i in range(5)],
            FakeCtx(),
        )


@pytest.mark.asyncio
async def test_malformed_question_rejected():
    with pytest.raises(ToolError):
        await ask_user(["not-a-dict"], FakeCtx())  # type: ignore[list-item]
