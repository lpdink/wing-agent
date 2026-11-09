"""会话物化（materialization）：第一条用户消息之前，磁盘上零痕迹。

不变量（本次变更的核心；口径见 ``docs/dev/usage.md`` 与
``docs/dev/architecture.md`` 的会话生命周期一节）：

- 新建会话在第一条用户消息到来前**只是内存草稿**：不建目录、不写
  ``metadata.json`` / ``history.jsonl``，也不写用量目录（``session_catalog``）；
- 创建期的 hook 注入 / 覆盖 / 打标 / 模型切换都留在草稿里，随第一条消息
  一次落盘（整份 metadata，一次写）；
- 既有会话（resume / fork 的子会话）构造时已在盘上，显式动作照常立即落盘；
- 显式 id 的「认领目录」（别名归一）仍然发生，但空目录不是会话。

TUI 开了不用的空会话因此不再出现在磁盘与用量统计里 —— 这是本文件存在的理由。
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from _support import seed_messages
from wing.db import get_database
from wing.hooks import hooks
from wing.session import AgentOverride, SessionManager
from wing.store import FileSessionStore

# ============================================================
# fixtures
# ============================================================


@pytest.fixture
def root(tmp_path: Path) -> Path:
    """文件后端存储根目录。"""
    return tmp_path / "sessions"


@pytest.fixture
def sm(root: Path) -> SessionManager:
    return SessionManager({"file": FileSessionStore(root)})


def _restart(root: Path) -> SessionManager:
    """同一存储根上的新 manager（模拟网关重启）。"""
    return SessionManager({"file": FileSessionStore(root)})


def _raw(root: Path, session_id: str) -> dict[str, Any]:
    return json.loads((root / session_id / "metadata.json").read_text(encoding="utf-8"))


async def _post(session: Any, content: str = "hello") -> None:
    """投递一条用户消息（停掉 worker：这些用例不跑 turn、不打网络）。"""
    session.agent._worker.cancel()
    await session.post(content)


def _catalog_rows() -> list[Any]:
    return get_database().query("SELECT * FROM session_catalog")


# ============================================================
# 第一条消息之前：零痕迹
# ============================================================


class TestNothingBeforeTheFirstMessage:
    @pytest.mark.asyncio
    async def test_create_leaves_no_trace(self, sm: SessionManager, root: Path) -> None:
        """自生成 id 的会话：连目录都不建（没有认领动作）。"""
        session = sm.create_session()

        assert not (root / session.session_id).exists()
        assert sm.list_sessions() == []
        assert _catalog_rows() == []

    @pytest.mark.asyncio
    async def test_explicit_id_claims_the_dir_but_is_not_a_session(
        self, sm: SessionManager, root: Path
    ) -> None:
        """显式 id 会认领目录（别名归一的机制），但空目录不是会话。"""
        session = sm.create_session(session_id="20250102-030405-abcdef")

        assert (root / session.session_id).is_dir()
        assert not (root / session.session_id / "metadata.json").exists()
        assert FileSessionStore(root).exists(session.session_id) is False
        assert sm.list_sessions() == []

    @pytest.mark.asyncio
    async def test_hook_injection_waits_for_the_first_message(
        self, sm: SessionManager, root: Path
    ) -> None:
        """hook 注入的追加系统提示词是草稿：首条消息之前不落盘。"""

        def hook_inject(session: Any, **ctx: Any) -> None:  # noqa: ANN401
            session.context_manager.append_to_system_prompt("<os>probe</os>")

        hooks.on("before_session_start")(hook_inject)
        try:
            session = sm.create_session()
            sid = session.session_id
            assert not (root / sid).exists()

            await _post(session)

            assert _raw(root, sid)["append_system_prompt"] == "<os>probe</os>"
        finally:
            assert hooks.off("before_session_start", hook_inject)

    @pytest.mark.asyncio
    async def test_explicit_actions_before_the_first_message_stay_draft(
        self, sm: SessionManager, root: Path
    ) -> None:
        """覆盖 / 打标 / 模型切换都留在内存：会话仍是草稿，磁盘零痕迹。"""
        session = sm.create_session(
            agent_override=AgentOverride(model_id="qwen3-max"), tags=["draft"]
        )
        sid = session.session_id
        await session.update_state(model_id="gpt-4o-mini")

        assert not (root / sid).exists()
        assert _catalog_rows() == []

        await _post(session, "hi")

        raw = _raw(root, sid)
        assert raw["model_id"] == "gpt-4o-mini"
        assert raw["tags"] == ["draft"]
        assert raw["session_name"] == "hi"

    @pytest.mark.asyncio
    async def test_catalog_row_waits_for_the_first_message(
        self, sm: SessionManager
    ) -> None:
        """用量目录（派生投影）跟着物化走：空会话不写行。"""
        session = sm.create_session(parent_session_id="root-a")
        assert _catalog_rows() == []

        await _post(session, "hi")

        rows = _catalog_rows()
        assert [row["session_id"] for row in rows] == [session.session_id]
        assert rows[0]["parent_session_id"] == "root-a"


# ============================================================
# 第一条消息：一次落盘
# ============================================================


class TestTheFirstMessageMaterializes:
    @pytest.mark.asyncio
    async def test_one_write_carries_title_lineage_and_template(
        self, sm: SessionManager, root: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """标题 / 血缘 / 模板 / last_interaction 合并进**一次** metadata 写。"""
        session = sm.create_session(
            workspace=str(root.parent), parent_session_id="root-a", tags=["crew"]
        )
        sid = session.session_id

        writes: list[dict[str, Any]] = []
        original = session.store.save_metadata

        def spy(session_id: str, metadata: Any) -> None:
            writes.append(metadata.model_dump(exclude_none=True))
            original(session_id, metadata)

        monkeypatch.setattr(session.store, "save_metadata", spy)
        await _post(session, "first words")

        assert len(writes) == 1
        raw = _raw(root, sid)
        assert raw["session_name"] == "first words"
        assert raw["parent_session_id"] == "root-a"
        assert raw["tags"] == ["crew"]
        assert raw["template_name"] == "default"
        assert raw["last_interaction"]

    @pytest.mark.asyncio
    async def test_history_appears_with_the_first_message(
        self, sm: SessionManager, root: Path
    ) -> None:
        """``history.jsonl`` 只在有消息时出现（空会话永远没有它）。"""
        session = sm.create_session()
        sid = session.session_id
        assert not (root / sid / "history.jsonl").exists()

        await _post(session, "hello")
        seed_messages(session, "hello")

        lines = (root / sid / "history.jsonl").read_text(encoding="utf-8").splitlines()
        assert lines, "first message must be durably recorded"


# ============================================================
# 写失败 / 摘除：闸门必须可重试、不许留下第四条写路径
# ============================================================


class TestTheGateIsFailSafe:
    @pytest.mark.asyncio
    async def test_a_failed_first_write_keeps_the_session_a_draft(
        self, sm: SessionManager, root: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """首条消息落盘失败 → 仍是草稿（下一次 post 重试，catalog 不提前登记）。"""
        session = sm.create_session()
        sid = session.session_id
        session.agent._worker.cancel()

        original = session.store.save_metadata
        failures = {"left": 1}

        def flaky(session_id: str, metadata: Any) -> None:
            if failures["left"]:
                failures["left"] -= 1
                raise OSError("simulated disk full")
            original(session_id, metadata)

        monkeypatch.setattr(session.store, "save_metadata", flaky)

        with pytest.raises(OSError):
            await session.post("first")
        assert session.materialized is False
        assert not (root / sid / "metadata.json").exists()
        assert _catalog_rows() == []

        await session.post("again")

        assert session.materialized is True
        # 没送出去的那条不该留下标题：标题跟着真正让会话出生的消息走。
        assert _raw(root, sid)["session_name"] == "again"
        assert [row["session_id"] for row in _catalog_rows()] == [sid]

    @pytest.mark.asyncio
    async def test_a_detached_session_never_materializes(
        self, sm: SessionManager, root: Path
    ) -> None:
        """摘除后 `post` 不写 metadata、也不写 catalog（第四条写路径被封住）。"""
        session = sm.create_session()
        sid = session.session_id
        session.agent._worker.cancel()
        session.detach()

        await session.post("too late")

        assert session.materialized is False
        assert not (root / sid).exists()
        assert _catalog_rows() == []


# ============================================================
# 持久消费者的显式承诺
# ============================================================


class TestDurableConsumers:
    @pytest.mark.asyncio
    async def test_ensure_durable_materializes_a_draft(
        self, sm: SessionManager, root: Path
    ) -> None:
        """登记唤醒 = 对「活到重启之后」的承诺：草稿就地物化（不是拒绝、不是空转）。"""
        session = sm.create_session(parent_session_id="root-a")
        sid = session.session_id
        assert not (root / sid).exists()

        sm.ensure_durable(sid)

        assert session.materialized is True
        raw = _raw(root, sid)
        assert raw["template_name"] == "default"
        assert raw["parent_session_id"] == "root-a"
        # 第一条消息还没来：没有标题 / 互动时间（不发明事实）。
        assert "session_name" not in raw
        assert "last_interaction" not in raw
        assert [row["session_id"] for row in _catalog_rows()] == [sid]

        # 幂等：第二次是 no-op。
        sm.ensure_durable(sid)
        assert len(_catalog_rows()) == 1

    @pytest.mark.asyncio
    async def test_ensure_durable_on_an_unknown_session_raises(
        self, sm: SessionManager
    ) -> None:
        with pytest.raises(LookupError):
            sm.ensure_durable("no-such-session")


# ============================================================
# 既有会话不受闸门影响
# ============================================================


class TestExistingSessionsPassTheGate:
    @pytest.mark.asyncio
    async def test_resume_writes_explicit_actions_immediately(
        self, sm: SessionManager, root: Path
    ) -> None:
        """resume 回来的会话已物化：显式动作照常立即落盘（跨重启不丢）。"""
        session = sm.create_session()
        await _post(session)
        sid = session.session_id

        resumed = _restart(root).resume_session(sid)
        await resumed.update_state(model_id="qwen3-max")

        raw = _raw(root, sid)
        assert raw["model_id"] == "qwen3-max"

    @pytest.mark.asyncio
    async def test_fork_child_is_born_materialized(
        self, sm: SessionManager, root: Path
    ) -> None:
        """fork 的子会话生来带着历史：显式动作与首条消息都立即落盘。"""
        source = sm.create_session()
        await _post(source, "hello")
        seed_messages(source, "hello", "hi")
        window = source.context_manager.get_context_window()
        assert window, "source must have a message to fork at"
        target = window[-1].uuid
        assert target is not None

        forked = sm.fork_session(source.session_id, target)
        assert forked is not None
        child, _ = forked
        assert (root / child.session_id / "metadata.json").exists()

        await child.update_state(model_id="qwen3-max")
        assert _raw(root, child.session_id)["model_id"] == "qwen3-max"

        # 子会话没有「草稿期」：首条消息的标题与互动时间在 post 里直接落盘
        # （`materialize` 对它是 no-op，回归面见 fork 的标题测试）。
        child.agent._worker.cancel()
        await child.post("child first words")
        raw = _raw(root, child.session_id)
        assert raw["session_name"] == "child first words"
        assert raw["forked_from"] == source.session_id
