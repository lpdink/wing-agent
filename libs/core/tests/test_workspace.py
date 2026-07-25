"""Session.set_workspace() 单元测试。

测试路径校验、agent cwd 更新、ContextManager 同步、metadata 持久化。
"""

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from wing.agent_state_bag import AgentStateBag


def _make_session(tmp_path: Path, workspace: str | None = None):
    """构建最小化 Session 实例用于 set_workspace 测试。"""
    from wing.session import Session
    from wing.store import FileSessionStore

    mock_agent = MagicMock()
    mock_agent.state = AgentStateBag()
    if workspace:
        mock_agent.state.set("cwd", workspace)

    mock_cm = MagicMock()
    mock_cm._workspace = Path(workspace) if workspace else None

    session = Session(
        session_id="test-ws",
        messages=MagicMock(),
        context_manager=mock_cm,
        agent=mock_agent,
        store=FileSessionStore(tmp_path / "sessions"),
        workspace=workspace,
    )

    return session


class TestSetWorkspace:
    def test_valid_directory(self, tmp_path: Path):
        """合法目录：更新 workspace、agent cwd、CM workspace、metadata。"""
        target = tmp_path / "project"
        target.mkdir()

        session = _make_session(tmp_path, workspace=str(tmp_path))
        session.set_workspace(str(target))

        assert session.session_workspace == str(target)
        assert session.agent.state.get("cwd") == str(target)
        assert session._context_manager._workspace == target

        # metadata.json 已持久化
        meta_path = tmp_path / "sessions" / "test-ws" / "metadata.json"
        assert meta_path.exists()
        import json

        data = json.loads(meta_path.read_text())
        assert data["workspace"] == str(target)

    def test_nonexistent_path(self, tmp_path: Path):
        """不存在的路径抛 ValueError。"""
        session = _make_session(tmp_path, workspace=str(tmp_path))

        with pytest.raises(ValueError, match="does not exist"):
            session.set_workspace(str(tmp_path / "nonexistent"))

        # workspace 未变
        assert session.session_workspace == str(tmp_path)

    def test_file_path_not_directory(self, tmp_path: Path):
        """文件路径（非目录）抛 ValueError。"""
        file_path = tmp_path / "file.txt"
        file_path.write_text("hello")

        session = _make_session(tmp_path, workspace=str(tmp_path))

        with pytest.raises(ValueError, match="not a directory"):
            session.set_workspace(str(file_path))

        assert session.session_workspace == str(tmp_path)

    def test_tilde_expansion(self, tmp_path: Path):
        """~ 路径正确展开。"""
        session = _make_session(tmp_path, workspace=str(tmp_path))

        with patch("pathlib.Path.expanduser") as mock_expand:
            mock_expand.return_value = tmp_path
            session.set_workspace("~/some_dir")

        assert session.session_workspace == str(tmp_path)

    def test_relative_path_resolved(self, tmp_path: Path):
        """相对路径被 resolve 为绝对路径。"""
        sub = tmp_path / "sub"
        sub.mkdir()

        session = _make_session(tmp_path, workspace=str(tmp_path))

        import os

        old_cwd = os.getcwd()
        try:
            os.chdir(tmp_path)
            session.set_workspace("sub")
        finally:
            os.chdir(old_cwd)

        assert session.session_workspace == str(sub)
        assert Path(session.session_workspace).is_absolute()
