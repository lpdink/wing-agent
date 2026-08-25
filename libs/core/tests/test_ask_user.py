"""测试 AskUserQuestion 工具：返回自描述结构、回答原样透传、不解析用户文本。"""

import json

import pytest

from wing.tools.ask_user import (
    AskQuestion,
    _build_enriched_response,
    _label_choices,
    ask_user,
)


class FakeCtx:
    """最小 ToolContext 桩：ask_feedback 返回预设的原始反馈 JSON。"""

    def __init__(self, response: str):
        self._response = response

    @property
    def session_id(self) -> str:
        return "sess-test"

    async def ask_feedback(self, event, timeout: float) -> str:
        return self._response


def test_label_choices_from_position():
    """字母标签按位置推导（A/B/C），与 TUI 渲染一致；空列表返回空。"""
    assert _label_choices(["one", "two", "three"]) == [
        "A. one",
        "B. two",
        "C. three",
    ]
    assert _label_choices([]) == []


def test_build_enriched_answer_verbatim_and_choices_echoed():
    """用户自由文本原样透传；带标签 choices 回显供语义对齐。"""
    qs = [
        AskQuestion(id="q1", question="first?", choices=["one", "two"]),
        AskQuestion(id="q2", question="second?", choices=[]),
    ]
    fb = json.dumps({"q1": "Agent 方案", "q2": "之后做。"})
    out = json.loads(_build_enriched_response(qs, fb))

    # "Agent" 以 A 开头 —— 必须原样透传，绝不能被解析成 A 选项
    assert out["q1"]["answer"] == "Agent 方案"
    assert out["q1"]["choices"] == ["A. one", "B. two"]
    assert out["q2"]["answer"] == "之后做。"
    assert out["q2"]["choices"] == []
    # answer 与 choices 语义对齐：裸字母 "A" 可映射到 "A. one"
    assert out["q1"]["answer"].split()[0] == "Agent"
    assert "one" in out["q1"]["choices"][0]


def test_build_enriched_fallback_on_bad_json():
    """非 JSON 反馈原样返回（不崩溃、不解析）。"""
    qs = [AskQuestion(id="q1", question="first?", choices=["one"])]
    assert _build_enriched_response(qs, "not json at all") == "not json at all"
    assert _build_enriched_response(qs, None) == "{}"


@pytest.mark.asyncio
async def test_ask_user_full_flow():
    """完整链路：发出问题 → 收到反馈 → 返回自描述结构。"""
    questions = [
        {"id": "q1", "question": "First?", "choices": ["one", "two"]},
        {"id": "q2", "question": "Second?", "choices": []},
    ]
    ctx = FakeCtx(json.dumps({"q1": "A", "q2": "自由回答"}))

    result = await ask_user(questions, ctx)
    out = json.loads(result)
    assert out["q1"]["answer"] == "A"
    assert out["q1"]["choices"] == ["A. one", "B. two"]
    assert out["q2"]["answer"] == "自由回答"
    assert out["q2"]["choices"] == []