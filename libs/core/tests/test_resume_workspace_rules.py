"""resume 工作目录回归测试。

resume_session 曾漏传 workspace 给 from_template，导致 ContextManager 以
workspace=None 构建、在 __init__ 里急切加载 rules/skills 时相对路径退化到
进程 cwd 展开，把错误文件（或空）加载进系统提示词。Session.__init__ 的事后
回填只设了 _workspace 字段、救不回已缓存的 rules/skills，因此极具迷惑性——
既有的 resume 测试只断言 _workspace 字段，恰好漏过了真正坏掉的部分。

本测试断言「实际加载的 rules 内容」而非 _workspace 字段，钉死该路径。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.session_manager import SessionManager
from wing.store import FileSessionStore

_MARKER = "UNIQUE_RULE_MARKER_42"


@pytest.mark.asyncio
async def test_resume_loads_rules_from_workspace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, _mock_config
) -> None:
    """resume 后 rules 仍从持久化的 workspace 加载，而非进程 cwd。"""
    # 干净 cwd：缺失 workspace 时相对 pattern 绝无可能误匹配到文件。
    cwd = tmp_path / "cwd"
    cwd.mkdir()
    monkeypatch.chdir(cwd)

    # 给默认模板一个相对路径 rules pattern——其解析基准正是 workspace。
    _mock_config.agents[0].rules = ["RULES.md"]

    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "RULES.md").write_text(_MARKER, encoding="utf-8")

    sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
    session = sm.create_session(workspace=str(ws))
    # set_workspace 持久化 metadata.workspace（现实里由 TUI 经 gateway 触发），
    # 并让 session 落盘、可被全新 SM 实例 resume。
    session.set_workspace(str(ws))

    # sanity：新建 session 从自己的 workspace 加载 rules。
    assert _MARKER in session.context_manager._rules_prompt

    # 模拟进程重启：同一 store 根目录 + 同一 config，全新 SM 实例。
    restarted = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
    resumed = restarted.resume_session(session.session_id)

    # 修复前：resume 以 workspace=None 构建 CM，"RULES.md" 对 cwd 展开匹配不到
    # 任何文件，_rules_prompt 为空 → 此断言失败。修复后从持久化的 ws 加载。
    assert _MARKER in resumed.context_manager._rules_prompt
