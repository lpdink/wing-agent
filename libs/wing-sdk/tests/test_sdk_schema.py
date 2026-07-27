"""schema 序列化测试。"""

from wing_sdk.schema import RemoteToolSpec, ToolParam


def test_tool_param_minimal():
    d = ToolParam(name="path").to_dict()
    assert d == {"name": "path", "type": "string"}


def test_tool_param_full():
    d = ToolParam(
        name="timeout",
        type="integer",
        description="Max seconds.",
        default=30,
    ).to_dict()
    assert d == {
        "name": "timeout",
        "type": "integer",
        "description": "Max seconds.",
        "default": 30,
    }


def test_tool_param_false_default_kept():
    # default=False 是合法默认值，不应被省略规则吞掉
    d = ToolParam(name="flag", type="boolean", default=False).to_dict()
    assert d["default"] is False


def test_tool_param_items():
    d = ToolParam(name="xs", type="array", items="string").to_dict()
    assert d["items"] == "string"


def test_remote_spec_omits_empty():
    d = RemoteToolSpec(name="Bash").to_dict()
    assert d == {"name": "Bash"}
    assert "description" not in d
    assert "llm_name" not in d
    assert "params" not in d


def test_remote_spec_full():
    spec = RemoteToolSpec(
        name="exec",
        description="Run stuff",
        llm_name="Bash",
        params=[ToolParam(name="command")],
    )
    d = spec.to_dict()
    assert d["name"] == "exec"
    assert d["llm_name"] == "Bash"
    assert d["params"] == [{"name": "command", "type": "string"}]
