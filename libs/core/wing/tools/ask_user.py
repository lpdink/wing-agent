"""Ask user for feedback during agent execution.

This tool allows the agent to pause and ask the user one or more questions,
collecting structured answers before continuing.
"""

import asyncio

from pydantic import BaseModel, Field

from wing.agent import WingAgent
from wing.event import AskEvent
from wing.schema import ToolError, ToolParam
from wing.tool_registry import tool_registry

# Feedback timeout in seconds
FEEDBACK_TIMEOUT = 6000


class AskQuestion(BaseModel):
    """A single question in a multi-question ask."""

    id: str
    question: str
    choices: list[str] = Field(default_factory=list)


_QUESTIONS_PARAM = ToolParam(
    name="questions",
    type="array",
    items={
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Stable identifier for mapping answers (snake_case).",
            },
            "question": {
                "type": "string",
                "description": "The question to ask the user.",
            },
            "choices": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Optional suggested choices. User can still provide their own answer freely.",
            },
        },
        "required": ["id", "question"],
    },
    description=(
        "Questions to show the user. Prefer 1 and do not exceed 3. "
        "User answers each question freely (free-form text or picking a choice)."
    ),
)


@tool_registry.register(
    name="AskUserQuestion", add_purpose=False, params=[_QUESTIONS_PARAM]
)
async def ask_user(
    questions: list[dict],
    agent: WingAgent,
) -> str:
    """Ask user questions and wait for their responses.

    Use this tool when you need user input during execution:
    - Uncertain technical decisions
    - Conflicts with previous instructions
    - Need for requirement clarification
    - Presenting options for user to choose

    Args:
        questions: List of questions (1-3). Each has an id, question text,
            and optional choices.

    Returns:
        JSON mapping each question id to the user's answer.
    """
    # Normalize and validate questions.
    if not questions:
        raise ToolError("At least one question is required.")
    normalized: list[AskQuestion] = []
    seen_ids: set[str] = set()
    for i, q in enumerate(questions):
        qid = q.get("id") or f"q{i + 1}"
        question_text = q.get("question") or ""
        if not question_text.strip():
            raise ToolError(f"Question '{qid}' has empty question text.")
        if qid in seen_ids:
            raise ToolError(
                f"Duplicate question id: '{qid}'. Each question must have a unique id."
            )
        seen_ids.add(qid)
        normalized.append(
            AskQuestion(id=qid, question=question_text, choices=q.get("choices") or [])
        )

    # Set need_feedback state
    agent.state.set("need_feedback", True)

    # Send questions to user via EventBus
    agent.emit(
        AskEvent(
            session_id=agent.session_id,
            questions=[q.model_dump() for q in normalized],
        )
    )

    # Wait for user feedback (TUI sends structured JSON)
    try:
        feedback = await asyncio.wait_for(
            agent._inbox_feedback.get(), timeout=FEEDBACK_TIMEOUT
        )
        agent.state.set("need_feedback", False)
        return feedback if feedback else "{}"
    except asyncio.TimeoutError:
        agent.state.set("need_feedback", False)
        raise ToolError(
            "⚠️ Feedback timeout. User did not respond in time. "
            "Please proceed with your best judgment or try again."
        )
