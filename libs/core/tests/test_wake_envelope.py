"""tests/test_wake_envelope.py — 内置信封的逐字节形状。

信封是**产品文案**（用户会在会话里读到它，也会照着里面的命令复制），所以断言逐行：
done / ask（多问题）· ask（legacy 单问题）· timer · 2000 字符截断 · 用户 prompt 原样。
"""

from __future__ import annotations

from wing.wake.envelope import (
    MESSAGE_LIMIT,
    ask_envelope,
    done_envelope,
    session_payload,
    timer_envelope,
    truncate,
)
from wing.wake.model import WakeTrigger


def _trigger(**overrides: object) -> WakeTrigger:
    payload: dict[str, object] = {
        "id": 1,
        "kind": "session",
        "target_session_id": "parent",
        "source_session_id": "child",
        "on_events": "done,ask",
        "spec": None,
        "next_fire_at_ms": None,
        "remaining_done": 1,
        "prompt": None,
        "state": "active",
        "fire_count": 0,
        "last_fire_at_ms": None,
        "last_error": None,
        "created_at_ms": 0,
        "created_by": None,
    }
    payload.update(overrides)
    return WakeTrigger(**payload)  # type: ignore[arg-type]


class TestDoneEnvelope:
    def test_exact_shape(self) -> None:
        text = done_envelope(
            session_id="20260101-120000-abcd",
            subtype="success",
            num_turns=3,
            duration_ms=12_345,
            result="patched the parser",
        )
        assert text == (
            "[wing] session 20260101-120000-abcd finished "
            "(subtype=success, turns=3, 12.3s)\n"
            "\n"
            "Last message:\n"
            "patched the parser\n"
            "\n"
            "Inspect:   wing tail 20260101-120000-abcd\n"
            'Continue:  wing run -r 20260101-120000-abcd -p "..."'
        )

    def test_without_result_says_no_text(self) -> None:
        """``error_during_execution`` 没有最终正文：写 ``(no text)`` 而不是空行。"""
        text = done_envelope(
            session_id="s1",
            subtype="error_during_execution",
            num_turns=0,
            duration_ms=500,
            result=None,
        )
        assert "Last message:\n(no text)\n" in text
        assert "subtype=error_during_execution" in text
        assert "0.5s" in text

    def test_blank_result_is_no_text(self) -> None:
        text = done_envelope(
            session_id="s1",
            subtype="success",
            num_turns=1,
            duration_ms=0,
            result="   \n ",
        )
        assert "(no text)" in text

    def test_long_result_is_truncated(self) -> None:
        text = done_envelope(
            session_id="s1",
            subtype="success",
            num_turns=1,
            duration_ms=0,
            result="x" * 5000,
        )
        body = text.split("Last message:\n")[1].split("\n\nInspect:")[0]
        assert len(body) == MESSAGE_LIMIT + 1  # 2000 正文 + 省略号
        assert body.endswith("…")


class TestAskEnvelope:
    def test_multi_question(self) -> None:
        text = ask_envelope(
            session_id="s1",
            tool_call_id="tc-9",
            questions=[
                {
                    "header": "Color",
                    "question": "Which color?",
                    "options": [{"label": "red"}, {"label": "blue"}],
                },
                {
                    "header": "Size",
                    "question": "Which size?",
                    "options": [{"label": "S"}, {"label": "L"}],
                },
            ],
        )
        assert text == (
            "[wing] session s1 is waiting for your answer (tool_call_id=tc-9)\n"
            "\n"
            "Question:\n"
            "- [Color] Which color?  (red / blue)\n"
            "- [Size] Which size?  (S / L)\n"
            "\n"
            'Answer: wing run -r s1 --tool-call-id tc-9 -p "<answer>"'
        )

    def test_question_without_options_or_header(self) -> None:
        text = ask_envelope(
            session_id="s1",
            tool_call_id="tc-1",
            questions=[{"header": "", "question": "Free form?"}],
        )
        assert "- Free form?\n" in text
        assert "(" not in text.split("Question:\n")[1].split("\n\n")[0]

    def test_legacy_single_question(self) -> None:
        """legacy（Bash 危险命令确认）：只有 question + choices。"""
        text = ask_envelope(
            session_id="s1",
            tool_call_id="tc-2",
            questions=[],
            question="Proceed?",
            choices=["y", "n", "yolo"],
        )
        assert "Question:\n- Proceed?  (y / n / yolo)\n" in text

    def test_empty_payload_still_renders(self) -> None:
        text = ask_envelope(session_id="s1", tool_call_id="tc-3")
        assert "Question:\n- (no text)\n" in text


class TestTimerEnvelope:
    def test_exact_shape(self) -> None:
        assert timer_envelope(
            spec="every:30m", ordinal=3, prompt="check the build"
        ) == ("[wing] scheduled wake (every:30m, #3)\n\ncheck the build")


class TestSessionPayload:
    def test_prompt_overrides_the_envelope(self) -> None:
        """用户给了 prompt → 原样发送（不做模板替换）。"""
        trigger = _trigger(prompt="heads up: child is done")
        assert session_payload(trigger, "envelope") == "heads up: child is done"

    def test_without_prompt_uses_the_envelope(self) -> None:
        assert session_payload(_trigger(), "envelope") == "envelope"


class TestTruncate:
    def test_short_text_untouched(self) -> None:
        assert truncate("abc") == "abc"

    def test_exact_limit_untouched(self) -> None:
        assert truncate("x" * MESSAGE_LIMIT) == "x" * MESSAGE_LIMIT

    def test_over_limit_marked(self) -> None:
        assert truncate("x" * (MESSAGE_LIMIT + 1)) == "x" * MESSAGE_LIMIT + "…"
