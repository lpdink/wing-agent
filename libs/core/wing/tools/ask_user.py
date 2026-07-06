"""Ask user for feedback during agent execution.

This tool allows the agent to pause and ask the user for clarification,
confirmation, or input when encountering uncertain decisions.
"""

import asyncio

from wing.agent import WingAgent
from wing.event import AskEvent
from wing.schema import ToolError
from wing.tool_registry import tool_registry

# Feedback timeout in seconds
FEEDBACK_TIMEOUT = 6000


@tool_registry.register(name="AskUserQuestion", add_purpose=False)
async def ask_user(
    question: str,
    agent: WingAgent,
    choices: list[str] | None = None,
) -> str:
    """Ask user a question and wait for their response.

    Use this tool when you need user input during execution:
    - Uncertain technical decisions
    - Conflicts with previous instructions
    - Need for requirement clarification
    - Presenting options for user to choose

    Args:
        question: The question to ask the user.
        choices: Optional list of suggested choices. User can still provide
            their own answer freely.

    Returns:
        User's response as a string.
    """
    # Set need_feedback state
    agent.state.set("need_feedback", True)

    # Send question to user via EventEmitter
    agent.emit(
        AskEvent(
            session_id=agent.session_id,
            question=question,
            choices=choices or [],
            required=False,
        )
    )

    # Wait for user feedback
    try:
        feedback = await asyncio.wait_for(
            agent._inbox_feedback.get(), timeout=FEEDBACK_TIMEOUT
        )
        agent.state.set("need_feedback", False)
        return feedback if feedback else ""
    except asyncio.TimeoutError:
        agent.state.set("need_feedback", False)
        raise ToolError(
            "⚠️ Feedback timeout. User did not respond in time. "
            "Please proceed with your best judgment or try again."
        )
