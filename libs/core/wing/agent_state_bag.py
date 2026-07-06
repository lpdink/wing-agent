# wing/agent_state_bag.py
"""Agent state storage for persisting tool results across calls."""

from typing import Any


class AgentStateBag:
    """A simple key-value store for agent state.

    Used by tools to persist results that influence future behavior.
    Example: Bash tool stores 'cwd' to maintain working directory.
    """

    def __init__(self) -> None:
        self._data: dict[str, object] = {}

    def get(self, key: str, default: Any = None) -> Any:
        """Get a state value by key.

        Args:
            key: The state key.
            default: Default value if key not found.

        Returns:
            The stored value or default.
        """
        if key in self._data:
            return self._data[key]
        return default

    def set(self, key: str, value: object) -> None:
        """Set a state value.

        Args:
            key: The state key.
            value: The value to store.
        """
        self._data[key] = value

    def delete(self, key: str) -> bool:
        """Delete a state key.

        Args:
            key: The state key to delete.

        Returns:
            True if key existed and was deleted, False otherwise.
        """
        if key in self._data:
            del self._data[key]
            return True
        return False

    def list_keys(self) -> list[str]:
        """List all state keys.

        Returns:
            List of all keys in the state bag.
        """
        return list(self._data.keys())

    def clear(self) -> None:
        """Clear all state."""
        self._data.clear()

    def __repr__(self) -> str:
        return f"AgentStateBag({self._data})"
