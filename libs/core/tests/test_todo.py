"""Tests for TodoWrite tool validation logic."""

import pytest

from wing.agent_state_bag import AgentStateBag
from wing.tools.todo import VALID_STATUSES, _validate_and_normalize, todo_write


class TestValidateAndNormalize:
    """Test the _validate_and_normalize helper."""

    def test_valid_items_pass_through(self):
        items = [
            {"content": "Run tests", "status": "in_progress"},
            {"content": "Fix bug", "status": "pending"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert len(result) == 2
        assert result[0]["content"] == "Run tests"
        assert result[0]["status"] == "in_progress"
        assert result[0]["activeForm"] == "Run tests"
        assert result[1]["content"] == "Fix bug"
        assert result[1]["activeForm"] == "Fix bug"
        assert warnings == []

    def test_activeForm_preserved_when_provided(self):
        items = [
            {
                "content": "Run tests",
                "status": "in_progress",
                "activeForm": "Running tests",
            },
        ]
        result, warnings = _validate_and_normalize(items)
        assert result[0]["activeForm"] == "Running tests"
        assert warnings == []

    def test_activeForm_defaults_to_content(self):
        items = [
            {"content": "Run tests", "status": "pending"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert result[0]["activeForm"] == "Run tests"

    def test_empty_content_skipped(self):
        items = [
            {"content": "", "status": "pending"},
            {"content": "Valid task", "status": "completed"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert len(result) == 1
        assert result[0]["content"] == "Valid task"
        assert any("empty 'content'" in w for w in warnings)

    def test_missing_content_skipped(self):
        items = [
            {"status": "pending"},
            {"content": "Valid task", "status": "completed"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert len(result) == 1
        assert any("missing or empty 'content'" in w for w in warnings)

    def test_whitespace_content_skipped(self):
        items = [
            {"content": "   ", "status": "pending"},
            {"content": "Valid task", "status": "completed"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert len(result) == 1
        assert any("empty 'content'" in w for w in warnings)

    def test_invalid_status_skipped(self):
        items = [
            {"content": "Bad task", "status": "unknown"},
            {"content": "Good task", "status": "pending"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert len(result) == 1
        assert result[0]["content"] == "Good task"
        assert any("invalid status" in w for w in warnings)

    def test_empty_status_skipped(self):
        items = [
            {"content": "No status", "status": ""},
            {"content": "Good task", "status": "pending"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert len(result) == 1
        assert any("invalid status" in w for w in warnings)

    def test_all_valid_statuses_accepted(self):
        for status in VALID_STATUSES:
            items = [{"content": "Task", "status": status}]
            result, warnings = _validate_and_normalize(items)
            assert len(result) == 1
            assert result[0]["status"] == status
            assert warnings == []

    def test_multiple_in_progress_auto_corrected(self):
        items = [
            {"content": "Task A", "status": "in_progress"},
            {"content": "Task B", "status": "in_progress"},
            {"content": "Task C", "status": "in_progress"},
        ]
        result, warnings = _validate_and_normalize(items)
        # Only the last in_progress should survive
        in_progress_items = [t for t in result if t["status"] == "in_progress"]
        pending_items = [t for t in result if t["status"] == "pending"]
        assert len(in_progress_items) == 1
        assert in_progress_items[0]["content"] == "Task C"
        assert len(pending_items) == 2
        assert any("Downgraded to pending" in w for w in warnings)

    def test_single_in_progress_no_warning(self):
        items = [
            {"content": "Task A", "status": "in_progress"},
            {"content": "Task B", "status": "pending"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert len(warnings) == 0
        assert result[0]["status"] == "in_progress"

    def test_all_items_invalid_returns_empty(self):
        items = [
            {"content": "", "status": "pending"},
            {"content": "Task", "status": "invalid"},
        ]
        result, warnings = _validate_and_normalize(items)
        assert len(result) == 0
        assert len(warnings) == 2

    def test_content_trimmed(self):
        items = [
            {"content": "  Run tests  ", "status": "pending"},
        ]
        result, _ = _validate_and_normalize(items)
        assert result[0]["content"] == "Run tests"

    def test_activeForm_trimmed(self):
        items = [
            {"content": "Task", "status": "pending", "activeForm": "  Running task  "},
        ]
        result, _ = _validate_and_normalize(items)
        assert result[0]["activeForm"] == "Running task"


class TestTodoWrite:
    """Test the todo_write async function."""

    @pytest.mark.asyncio
    async def test_basic_write(self):
        from unittest.mock import MagicMock

        agent = MagicMock()
        agent.state = AgentStateBag()

        result = await todo_write(
            [
                {"content": "Task A", "status": "in_progress"},
                {"content": "Task B", "status": "pending"},
            ],
            agent,
        )
        assert "Stay focused" in result
        assert "Task A" in result
        stored = agent.state.get("todo")
        assert len(stored) == 2

    @pytest.mark.asyncio
    async def test_all_completed_clears_list(self):
        from unittest.mock import MagicMock

        agent = MagicMock()
        agent.state = AgentStateBag()

        result = await todo_write(
            [
                {"content": "Task A", "status": "completed"},
                {"content": "Task B", "status": "completed"},
            ],
            agent,
        )
        assert "All todos completed" in result
        stored = agent.state.get("todo")
        assert stored == []

    @pytest.mark.asyncio
    async def test_all_invalid_returns_error_message(self):
        from unittest.mock import MagicMock

        from wing.schema import ToolError

        agent = MagicMock()
        agent.state = AgentStateBag()

        with pytest.raises(ToolError) as exc_info:
            await todo_write(
                [
                    {"content": "", "status": "pending"},
                    {"content": "Task", "status": "invalid"},
                ],
                agent,
            )
        assert "No valid todo items" in str(exc_info.value)

    @pytest.mark.asyncio
    async def test_batch_completion_warning(self):
        from unittest.mock import MagicMock

        agent = MagicMock()
        agent.state = AgentStateBag()
        # Pre-seed old todos
        agent.state.set(
            "todo",
            [
                {"content": "Task A", "status": "in_progress"},
                {"content": "Task B", "status": "pending"},
                {"content": "Task C", "status": "pending"},
            ],
        )

        result = await todo_write(
            [
                {"content": "Task A", "status": "completed"},
                {"content": "Task B", "status": "completed"},
                {"content": "Task C", "status": "completed"},
            ],
            agent,
        )
        assert "3 tasks as completed" in result

    @pytest.mark.asyncio
    async def test_warning_from_validation_appears_in_result(self):
        from unittest.mock import MagicMock

        agent = MagicMock()
        agent.state = AgentStateBag()

        result = await todo_write(
            [
                {"content": "Good task", "status": "pending"},
                {"content": "Bad task", "status": "invalid_status"},
            ],
            agent,
        )
        assert "Next task" in result
        assert "Good task" in result
        assert "invalid status" in result
