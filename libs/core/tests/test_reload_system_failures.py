"""reload_system 的降级语义：逐项失败各自成条，不互相拖累。

逐项名字序是对外契约（probe 钉住），这里补的是"某一步炸了会怎样"：
config 失败即中止；其余五项失败各自记一条 ok=False，继续往下跑。
"""

from __future__ import annotations

from typing import Any

import pytest

from wing.session.manager import SessionManager
from wing.store.file import FileSessionStore
from wing.system import reload_system

pytestmark = pytest.mark.asyncio


@pytest.fixture
def manager(tmp_path) -> SessionManager:
    return SessionManager({"file": FileSessionStore(tmp_path / "sessions")})


def _items(result) -> list[tuple[str, bool]]:
    return [(i.name, i.ok) for i in result.items]


async def test_config_failure_aborts_immediately(manager, monkeypatch):
    def _boom(*args: Any, **kwargs: Any):
        raise RuntimeError("config unreadable")

    monkeypatch.setattr("wing.config.load_config", _boom)

    result = await reload_system(manager)

    assert result.ok is False
    assert _items(result) == [("config.yaml", False)]
    assert "config unreadable" in (result.items[0].detail or "")


async def test_hooks_failure_is_isolated(manager, monkeypatch):
    monkeypatch.setattr("wing.config.load_config", lambda reload=False: _cfg())
    monkeypatch.setattr(
        "wing.hooks.load_hooks",
        lambda *a, **kw: (_ for _ in ()).throw(RuntimeError("hook blew up")),
    )

    result = await reload_system(manager)

    by_name = dict(_items(result))
    assert by_name["config.yaml"] is True
    assert by_name["hooks"] is False
    assert by_name["log level"] is True  # 后续步骤照常


async def test_prompt_commands_failure_is_isolated(manager, monkeypatch):
    monkeypatch.setattr("wing.config.load_config", lambda reload=False: _cfg())
    monkeypatch.setattr(
        "wing.commands.register_prompt_commands",
        lambda *a, **kw: (_ for _ in ()).throw(RuntimeError("bad glob")),
    )

    result = await reload_system(manager)

    by_name = dict(_items(result))
    assert by_name["prompt commands"] is False
    assert by_name["provider"] is True


async def test_skills_reload_failure_is_isolated(manager, monkeypatch, tmp_path):
    monkeypatch.setattr("wing.config.load_config", lambda reload=False: _cfg())
    session = manager.create_session(workspace=str(tmp_path))

    def _boom() -> None:
        raise RuntimeError("rules unreadable")

    monkeypatch.setattr(session.agent.context_manager, "reload_skills_and_rules", _boom)

    result = await reload_system(manager)

    by_name = dict(_items(result))
    assert by_name["skills & rules"] is False
    assert by_name["log level"] is True


async def test_log_level_failure_is_isolated(manager, monkeypatch):
    monkeypatch.setattr("wing.config.load_config", lambda reload=False: _cfg())
    monkeypatch.setattr(
        "wing.common.logger.setup_logger",
        lambda **kwargs: (_ for _ in ()).throw(RuntimeError("bad level")),
    )

    result = await reload_system(manager)

    by_name = dict(_items(result))
    assert by_name["log level"] is False
    assert result.ok is False  # 任何一项失败 → 整体 ok=False
    # 逐项名字序是对外契约
    assert [name for name, _ in _items(result)] == [
        "config.yaml",
        "hooks",
        "prompt commands",
        "provider",
        "skills & rules",
        "log level",
    ]


def _cfg():
    from wing.config import AgentConfig, Config, ProviderConfig, get_config

    try:
        return get_config()
    except Exception:
        return Config(
            providers=[
                ProviderConfig(
                    name="default",
                    base_url="https://api.example.com",
                    api_key="test",
                    models=["gpt-4"],
                )
            ],
            agents=[AgentConfig(name="default", model="gpt-4")],
        )
