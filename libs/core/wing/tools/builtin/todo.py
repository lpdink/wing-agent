"""TodoWrite tool for tracking task progress during a session.

Schema follows Claude's TodoWrite for LLM SFT compatibility,
but description is significantly trimmed to reduce context overhead.
"""

from wing.agent import ToolContext
from wing.schema import ToolError, ToolParam
from wing.tool_registry import tool_registry

# 模块级 todo 状态（session_id → todos）。仅 TodoWrite 工具读写。
_todo_store: dict[str, list[dict]] = {}

VALID_STATUSES = ("pending", "in_progress", "completed")

TODO_ITEM_SCHEMA = {
    "type": "object",
    "properties": {
        "content": {
            "type": "string",
            "description": "Task description in imperative form (e.g. 'Run tests')",
        },
        "status": {
            "type": "string",
            "enum": list(VALID_STATUSES),
            "description": "Task state: pending = not started, in_progress = currently working, completed = done",
        },
        "activeForm": {
            "type": "string",
            "description": "Present continuous form (e.g. 'Running tests'). Optional — defaults to content if omitted.",
        },
    },
    "required": ["content", "status"],
}

TODO_PARAMS = [
    ToolParam(
        name="todos",
        type="array",
        description="The updated todo list (replaces the entire previous list)",
        items=TODO_ITEM_SCHEMA,
    ),
]

_DESCRIPTION = (
    "Update the todo list for the current session. "
    "Use proactively for complex multi-step tasks (3+ steps). "
    "Rules: keep exactly ONE task as in_progress at a time; "
    "mark completed IMMEDIATELY after finishing; "
    "remove irrelevant tasks; "
    "provide content (imperative) and optionally activeForm (present continuous)."
)


def _validate_and_normalize(todos: list[dict]) -> tuple[list[dict], list[str]]:
    """Validate and normalize todo items.

    Returns (normalized_items, warnings).
    - Rejects items with empty/missing content or invalid status.
    - Auto-corrects multiple in_progress: keeps only the last, downgrades others to pending.
    - Defaults missing activeForm to content.
    """
    warnings: list[str] = []
    normalized: list[dict] = []

    for i, item in enumerate(todos):
        # --- content validation ---
        content = item.get("content")
        if not content or not isinstance(content, str) or not content.strip():
            warnings.append(f"Item {i}: missing or empty 'content', skipped.")
            continue
        content = content.strip()

        # --- status validation ---
        status = item.get("status", "")
        if status not in VALID_STATUSES:
            warnings.append(
                f"Item {i} ('{content}'): invalid status '{status}', "
                f"must be one of {VALID_STATUSES}, skipped."
            )
            continue

        # --- activeForm normalization ---
        active_form = item.get("activeForm")
        if active_form and isinstance(active_form, str):
            active_form = active_form.strip()
        if not active_form:
            active_form = content

        normalized.append(
            {"content": content, "status": status, "activeForm": active_form}
        )

    # --- single in_progress constraint ---
    in_progress_indices = [
        i for i, t in enumerate(normalized) if t["status"] == "in_progress"
    ]
    if len(in_progress_indices) > 1:
        # Keep only the last in_progress, downgrade others to pending
        for idx in in_progress_indices[:-1]:
            normalized[idx]["status"] = "pending"
        downgraded_names = [
            normalized[idx]["content"] for idx in in_progress_indices[:-1]
        ]
        warnings.append(
            f"Multiple in_progress tasks detected. "
            f"Downgraded to pending: {downgraded_names}. "
            "Only one task can be in_progress at a time."
        )

    return normalized, warnings


def _build_feedback_message(normalized: list[dict], all_done: bool) -> str:
    """Build context-aware feedback message based on todo state."""
    if all_done:
        return (
            "All todos completed. Please provide a summary of what was accomplished to the user. "
            "If you have already provided a summary, stop and wait for the user's next instruction."
        )

    in_progress = [t for t in normalized if t["status"] == "in_progress"]
    if in_progress:
        task = in_progress[0]
        active_form = task.get("activeForm", task["content"])
        return f"Todo updated. Currently {active_form}. Stay focused on this task."

    pending = [t for t in normalized if t["status"] == "pending"]
    if pending:
        next_task = pending[0]
        return f"Todo updated. Next task: {next_task['content']}. Start it when you're ready."

    return "Todos have been updated successfully."


@tool_registry.register(name="TodoWrite", params=TODO_PARAMS, description=_DESCRIPTION)
async def todo_write(todos: list[dict], ctx: ToolContext) -> str:
    """Update the todo list for the current session."""
    normalized, warnings = _validate_and_normalize(todos)

    # If nothing survived validation, raise error with warnings
    if not normalized:
        msg = "No valid todo items provided."
        if warnings:
            msg += " Issues: " + "; ".join(warnings)
        raise ToolError(msg)

    # Get old todos for comparison
    old_todos = _todo_store.get(ctx.session_id, [])

    # Detect batch completion: >=3 items changed to completed from in_progress/pending
    batch_completed_count = 0
    old_status_map: dict[str, str] = {}
    for t in old_todos:
        if isinstance(t, dict):
            c = t.get("content", "")
            s = t.get("status", "")
            old_status_map[str(c)] = str(s)
    for item in normalized:
        if item["status"] == "completed":
            old_status = old_status_map.get(item["content"])
            if old_status in ("in_progress", "pending"):
                batch_completed_count += 1

    # If all completed, clear the list (like Claude does)
    all_done = all(t["status"] == "completed" for t in normalized)
    new_todos = [] if all_done else normalized

    # Store normalized todos
    _todo_store[ctx.session_id] = new_todos

    # Build result message
    result = _build_feedback_message(normalized, all_done)
    if warnings:
        result += " " + "; ".join(warnings)
    if batch_completed_count >= 3:
        result += (
            f" NOTE: You just marked {batch_completed_count} tasks as completed "
            "at once. Make sure each was truly finished before closing them."
        )

    return result
