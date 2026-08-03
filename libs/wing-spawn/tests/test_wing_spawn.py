"""单元测试 for wing-spawn containers + runner 非端到端逻辑。

端到端（拉起容器 + 真实 gateway）由集成测试覆盖；这里只测纯逻辑。
"""

from __future__ import annotations


from wing_spawn.containers import _tool_refs
from wing_spawn.runner import SpawnRunner


class TestToolRefs:
    def test_list_of_strings(self):
        assert _tool_refs(["a.Bash", "b.Read"]) == ["a.Bash", "b.Read"]

    def test_dict_with_tools_key(self):
        payload = {"tools": [{"namespace": "host", "name": "Bash"}]}
        assert _tool_refs(payload) == ["host.Bash"]

    def test_dict_with_llm_name(self):
        payload = {"tools": [{"namespace": "host", "llm_name": "Bash"}]}
        assert _tool_refs(payload) == ["host.Bash"]

    def test_empty_and_malformed(self):
        assert _tool_refs({}) == []
        assert _tool_refs(None) == []
        assert _tool_refs({"tools": []}) == []


class TestSpawnRunner:
    def test_defaults(self):
        runner = SpawnRunner("hello")
        assert runner.goal_prompt == "hello"
        assert runner.client_id.startswith("task-")
        assert runner.model  # non-empty default
        assert runner.timeout == 1800.0
        assert runner.keep is False

    def test_client_id_override(self):
        runner = SpawnRunner("hello", client_id="my-task")
        assert runner.client_id == "my-task"

    def test_emit_result_success(self, capsys):
        runner = SpawnRunner("hello")
        code = runner._emit_result(
            {"result": "done!", "is_error": False, "num_turns": 3}
        )
        assert code == 0
        out = capsys.readouterr().out
        assert "3 turns" in out
        assert "done!" in out

    def test_emit_result_error(self, capsys):
        runner = SpawnRunner("hello")
        code = runner._emit_result(
            {
                "result": "boom",
                "is_error": True,
                "errors": ["timeout"],
                "num_turns": 1,
            }
        )
        assert code == 1
        out = capsys.readouterr().out
        assert "[error]" in out
        assert "timeout" in out