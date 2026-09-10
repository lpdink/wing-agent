"""Ask user for feedback during agent execution.

This tool allows the agent to pause and ask the user one or more questions
and wait for their answers.
"""

import asyncio
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from wing.agent import ToolContext
from wing.event import AskEvent
from wing.schema import ToolError, ToolParam
from wing.tool_registry import tool_registry

# Feedback timeout in seconds
FEEDBACK_TIMEOUT = 6000

# 用户在确认页选择 Cancel 时，前端发送的哨兵内容。工具把它翻译成明确的
# 取消说明（正常 tool result）——哨兵原文不会透传给模型。
ASK_CANCEL_TOKEN = "__wing_ask_cancelled__"
CANCEL_RESULT = "User cancelled the questions. Proceed with your best judgment."

MAX_QUESTIONS = 4
MAX_OPTIONS = 4
HEADER_MAX_CHARS = 12


class AskOption(BaseModel):
    """A selectable option: a label plus an optional longer description."""

    label: str
    description: str = ""


class AskQuestion(BaseModel):
    """A single question in a multi-question ask."""

    model_config = ConfigDict(populate_by_name=True)

    id: str
    header: str = ""
    question: str
    multi_select: bool = Field(default=False, alias="multiSelect")
    options: list[AskOption] = Field(default_factory=list)


_OPTIONS_ITEM = {
    "type": "object",
    "properties": {
        "label": {
            "type": "string",
            "description": "Option text shown to the user.",
        },
        "description": {
            "type": "string",
            "description": "Optional short explanation rendered under the label.",
        },
    },
    "required": ["label"],
}

_QUESTIONS_PARAM = ToolParam(
    name="questions",
    type="array",
    items={
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Stable identifier (snake_case) for mapping answers.",
            },
            "header": {
                "type": "string",
                "description": (
                    f"Very short label (at most {HEADER_MAX_CHARS} chars) "
                    "shown as the question tab."
                ),
            },
            "question": {
                "type": "string",
                "description": "The full question to ask the user.",
            },
            "multiSelect": {
                "type": "boolean",
                "description": "true = the user may pick several options.",
                "default": False,
            },
            "options": {
                "type": "array",
                "items": _OPTIONS_ITEM,
                "description": (
                    "2-4 options; each has a label and an optional description. "
                    "Omit for a free-form-only question."
                ),
            },
        },
        "required": ["id", "header", "question"],
    },
    description=(
        "1-4 questions to show the user. Prefer 1 and never exceed 4. "
        "The UI already offers a free-form 'Type Something' row and a final "
        "submit/cancel step — do not add options like 'Type Something', "
        "'Submit' or 'Cancel' yourself."
    ),
)


def _normalize_option(raw: Any, qid: str) -> AskOption:
    if isinstance(raw, str):
        raw = {"label": raw}
    if not isinstance(raw, dict):
        raise ToolError(f"Question '{qid}' has a malformed option: {raw!r}")
    label = str(raw.get("label") or "").strip()
    if not label:
        raise ToolError(f"Question '{qid}' has an option with an empty label.")
    return AskOption(label=label, description=str(raw.get("description") or "").strip())


def _normalize_question(raw: Any, index: int) -> AskQuestion:
    if not isinstance(raw, dict):
        raise ToolError(f"Question #{index + 1} is malformed: {raw!r}")
    qid = str(raw.get("id") or f"q{index + 1}").strip()
    question_text = str(raw.get("question") or "").strip()
    if not question_text:
        raise ToolError(f"Question '{qid}' has empty question text.")
    header = str(raw.get("header") or "").strip() or qid

    options_raw = raw.get("options")
    if not options_raw:
        # 兼容旧调用：choices 是纯字符串列表，等价于无描述的选项。
        options_raw = [{"label": c} for c in (raw.get("choices") or [])]
    options = [_normalize_option(opt, qid) for opt in options_raw]
    if len(options) > MAX_OPTIONS:
        raise ToolError(
            f"Question '{qid}' has {len(options)} options; at most "
            f"{MAX_OPTIONS} are allowed."
        )
    labels = [o.label for o in options]
    if len(set(labels)) != len(labels):
        raise ToolError(f"Question '{qid}' has duplicate option labels.")

    return AskQuestion(
        id=qid,
        header=header,
        question=question_text,
        multiSelect=bool(raw.get("multiSelect") or raw.get("multi_select") or False),
        options=options,
    )


@tool_registry.register(
    name="AskUserQuestion", add_purpose=False, params=[_QUESTIONS_PARAM]
)
async def ask_user(
    questions: list[dict],
    ctx: ToolContext,
) -> str:
    """Ask the user one or more multiple-choice questions and wait for answers.

    Use this tool when you need user input during execution:
    - Uncertain technical decisions
    - Conflicts with previous instructions
    - Need for requirement clarification
    - Presenting options for the user to choose

    Each question has a short `header` (rendered as the question tab), the
    full `question` text, and up to 4 `options` (each with a `label` and an
    optional `description`). Set `multiSelect=true` to let the user pick
    several options. Do NOT add options like "Type Something", "Submit" or
    "Cancel" — the UI always offers a free-form row and a final
    submit/cancel step by itself.

    The user's answers come back as text lines of the form `header: answer`,
    one line per question (multi-select answers list the chosen labels
    separated by commas). If the user cancels, the result is
    "User cancelled the questions. Proceed with your best judgment."

    Args:
        questions: 1-4 questions. Each has an id, a short header, the question
            text, optional 2-4 options, and an optional multiSelect flag.

    Returns:
        The user's answers as `header: answer` text lines.
    """
    if not questions:
        raise ToolError("At least one question is required.")
    if len(questions) > MAX_QUESTIONS:
        raise ToolError(
            f"Too many questions: {len(questions)} (at most {MAX_QUESTIONS})."
        )

    normalized: list[AskQuestion] = []
    seen_ids: set[str] = set()
    for i, raw in enumerate(questions):
        q = _normalize_question(raw, i)
        if q.id in seen_ids:
            raise ToolError(
                f"Duplicate question id: '{q.id}'. Each question must have a unique id."
            )
        seen_ids.add(q.id)
        normalized.append(q)

    # Ask the user and wait for the addressed response.
    # ask_feedback() 注入 tool_call_id 并 emit AskEvent，注册 feedback waiter，
    # 用户回复经 post(tool_call_id=...) 定向 resolve（并发 ask 互不干扰）。
    try:
        feedback = await ctx.ask_feedback(
            AskEvent(
                session_id=ctx.session_id,
                questions=[
                    q.model_dump(by_alias=True, exclude_none=True) for q in normalized
                ],
            ),
            timeout=FEEDBACK_TIMEOUT,
        )
    except asyncio.TimeoutError:
        raise ToolError(
            "⚠️ Feedback timeout. User did not respond in time. "
            "Please proceed with your best judgment or try again."
        )

    if (feedback or "").strip() == ASK_CANCEL_TOKEN:
        return CANCEL_RESULT
    return feedback or "(user gave no answer)"
