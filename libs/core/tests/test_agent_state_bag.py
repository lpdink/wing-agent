"""Tests for AgentStateBag."""

from wing.agent_state_bag import AgentStateBag


class TestAgentStateBag:
    """Test AgentStateBag functionality."""

    def test_set_and_get(self):
        """Test basic set and get operations."""
        bag = AgentStateBag()
        bag.set("cwd", "/tmp")
        assert bag.get("cwd") == "/tmp"

    def test_get_nonexistent_key(self):
        """Test getting a key that doesn't exist."""
        bag = AgentStateBag()
        assert bag.get("nonexistent") is None
        assert bag.get("nonexistent", "default") == "default"

    def test_delete_existing_key(self):
        """Test deleting an existing key."""
        bag = AgentStateBag()
        bag.set("key", "value")
        assert bag.delete("key") is True
        assert bag.get("key") is None

    def test_delete_nonexistent_key(self):
        """Test deleting a key that doesn't exist."""
        bag = AgentStateBag()
        assert bag.delete("nonexistent") is False

    def test_list_keys(self):
        """Test listing all keys."""
        bag = AgentStateBag()
        bag.set("cwd", "/tmp")
        bag.set("debug", True)
        keys = bag.list_keys()
        assert set(keys) == {"cwd", "debug"}

    def test_list_empty_bag(self):
        """Test listing keys on empty bag."""
        bag = AgentStateBag()
        assert bag.list_keys() == []

    def test_clear(self):
        """Test clearing all state."""
        bag = AgentStateBag()
        bag.set("key1", "value1")
        bag.set("key2", "value2")
        bag.clear()
        assert bag.list_keys() == []

    def test_overwrite_value(self):
        """Test overwriting an existing key."""
        bag = AgentStateBag()
        bag.set("cwd", "/tmp")
        bag.set("cwd", "/home")
        assert bag.get("cwd") == "/home"

    def test_various_value_types(self):
        """Test storing different value types."""
        bag = AgentStateBag()
        bag.set("string", "value")
        bag.set("int", 42)
        bag.set("bool", True)
        bag.set("list", [1, 2, 3])
        bag.set("dict", {"nested": "value"})

        assert bag.get("string") == "value"
        assert bag.get("int") == 42
        assert bag.get("bool") is True
        assert bag.get("list") == [1, 2, 3]
        assert bag.get("dict") == {"nested": "value"}

    def test_repr(self):
        """Test string representation."""
        bag = AgentStateBag()
        bag.set("cwd", "/tmp")
        repr_str = repr(bag)
        assert "AgentStateBag" in repr_str
        assert "cwd" in repr_str
