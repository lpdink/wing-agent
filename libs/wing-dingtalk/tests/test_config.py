"""Config 单测——会话工具集构造。"""

from wing_dingtalk.config import Config


def test_session_tools_default() -> None:
    cfg = Config()
    tools = cfg.session_tools()
    # 六件套(devbox 命名空间) + SendFile(前端命名空间) + TodoWrite 锚
    assert tools[:6] == [
        "devbox.Bash",
        "devbox.Read",
        "devbox.Write",
        "devbox.Edit",
        "devbox.Glob",
        "devbox.Grep",
    ]
    assert "dingtalk.SendFile" in tools
    # TodoWrite 锚：保证 resume 热切换后 LLM tools 参数非空
    assert "TodoWrite" in tools


def test_session_tools_custom_namespaces() -> None:
    cfg = Config(tool_host_ns="box", tool_client_id="dt")
    tools = cfg.session_tools()
    assert "box.Bash" in tools
    assert "dt.SendFile" in tools
    assert "TodoWrite" in tools


def test_session_tools_override() -> None:
    cfg = Config(session_tools_override=["a.Bash", "TodoWrite"])
    assert cfg.session_tools() == ["a.Bash", "TodoWrite"]


def test_required_tool_refs_default() -> None:
    cfg = Config()
    assert cfg.required_tool_refs() == ["devbox.Bash", "dingtalk.SendFile"]


def test_required_tool_refs_override() -> None:
    cfg = Config(expected_tool_refs=["x.Bash"])
    assert cfg.required_tool_refs() == ["x.Bash"]
