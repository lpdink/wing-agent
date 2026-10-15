"""归档（archive）的会话层语义：显式归档 / 恢复、冻结、sweep、豁免与提示。

被守的语义（``SessionManager.archive_session / unarchive_session /
archive_stale_sessions / list_archived_sessions``）：

- **冻结**：归档区不参与解析——resume / tag / release / create-or-adopt 一律
  以「已归档」结束（``SessionArchivedError``，可行动提示），绝不隐式恢复；
- **纯移动**：归档 → 恢复往返之后链与磁盘字节逐字一致（红线）；
- **先逐出再移动**（显式归档）：内存中的会话归档前先摘除（无活 Session 写旧
  路径）；钉住条件（忙碌 / 排队输入 / 被订阅）命中 → RuntimeError；
- **sweep 只管不在内存里的会话**：在内存的一律不动（内存回收是逐出 job 的
  职责），逐出后重新成为磁盘会话；
- **sweep**：只动「年龄已知且超过阈值」的会话；带 ``pin`` 标签的豁免；
  ``idle_days <= 0`` = 关闭；年龄未知（自定义 id 且无 last_interaction）不动；
- **列表投影**：归档条目把标题 / tags / 模型带出来（与 ps 同一读法）。
"""

from __future__ import annotations

import asyncio
import json
import shutil
from datetime import datetime, timedelta
from pathlib import Path

import pytest

from wing.event_bus import event_bus
from wing.schema import Message
from wing.session import SessionArchivedError, SessionManager
from wing.store import FileSessionStore, MemorySessionStore, SessionMetadata

pytestmark = pytest.mark.asyncio

CUSTOM_ID = "custom-id-without-timestamp"


@pytest.fixture
def root(tmp_path: Path) -> Path:
    return tmp_path / "sessions"


@pytest.fixture
def file_store(root: Path) -> FileSessionStore:
    return FileSessionStore(root)


@pytest.fixture
def sm(file_store: FileSessionStore) -> SessionManager:
    return SessionManager(
        {"file": file_store, "memory": MemorySessionStore()}, default_backend="file"
    )


def _history(root: Path, session_id: str) -> bytes:
    return (root / session_id / "history.jsonl").read_bytes()


def _archived_history(root: Path, session_id: str) -> bytes:
    return (root / ".archive" / session_id / "history.jsonl").read_bytes()


def _seed_old(
    store: FileSessionStore,
    session_id: str,
    *,
    days_ago: float | None,
    tags: list[str] | None = None,
) -> None:
    """在磁盘上造一个会话（可选把 last_interaction 拨旧）。"""
    last = (
        (datetime.now() - timedelta(days=days_ago)).isoformat()
        if days_ago is not None
        else None
    )
    store.save_metadata(
        session_id,
        SessionMetadata(
            session_name=f"title-{session_id}", last_interaction=last, tags=tags or None
        ),
    )
    store.open_log(session_id).append(
        [{"role": "user", "content": "hello", "uuid": "u1"}]
    )


class TestExplicitArchive:
    async def test_explicit_archive_refuses_both_exists_without_side_effects(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        """双存在预检**先于**逐出：抛错时不得留下"已逐出但没归档"的半截状态。"""
        _seed_old(file_store, "old", days_ago=1)
        sm = SessionManager({"file": file_store})
        session = sm.resume_session("old")  # 换入内存
        # 手工造出双存在（异常态）。
        (root / ".archive" / "old").mkdir(parents=True)
        (root / ".archive" / "old" / "history.jsonl").write_text("")

        with pytest.raises(RuntimeError, match="both"):
            sm.archive_session("old")

        assert sm.get_session("old") is session, "预检失败不该先逐出（半副作用）"
        assert (root / "old").exists()
        await sm.wait_teardowns()

    async def test_roundtrip_keeps_history_bytes_and_chain(
        self, root: Path, sm: SessionManager, file_store: FileSessionStore
    ) -> None:
        """归档往返是纯移动：链内容逐字一致，恢复后照常 resume（红线）。"""
        session = sm.create_session()
        sid = session.session_id
        session.context_manager.add_message(Message(role="user", content="hello"))
        before = _history(root, sid)
        chain_before = [
            node.uuid for node in session.context_manager.get_context_window()
        ]

        changed, detail = sm.archive_session(sid)
        assert (changed, detail) == (True, "archived")
        assert not (root / sid).exists()
        assert _archived_history(root, sid) == before
        assert sm.get_session(sid) is None  # 先逐出再移动

        # 冻结：任何按 id 的解析都以"已归档"结束。
        with pytest.raises(SessionArchivedError) as excinfo:
            sm.resume_session(sid)
        assert sid in str(excinfo.value)
        with pytest.raises(SessionArchivedError):
            sm.set_session_tags(sid, add=["x"])
        with pytest.raises(SessionArchivedError):
            sm.release_session(sid)
        assert sm.session_exists(sid) is False

        # 幂等：重复归档不是错误。
        assert sm.archive_session(sid) == (False, "already archived")

        # 列表：live 里没有它，归档列表里有它（标题 / 模型带出来）。
        assert sid not in [s.id for s in sm.list_sessions()]
        entries = sm.list_archived_sessions()
        assert [e.id for e in entries] == [sid]
        assert entries[0].name == "hello"  # 标题回退到首条用户消息
        assert entries[0].model_name  # 模型列有素材（与 ps 同一读法）

        # 恢复：history 字节一致 + 链一致 + 可以继续用。
        assert sm.unarchive_session(sid) == (True, "restored")
        assert _history(root, sid) == before
        assert sm.unarchive_session(sid) == (False, "not archived")
        # 恢复刷新 last_interaction（显式动作 = 兴趣信号；见
        # TestRestoredSessionSurvivesNextSweep）。
        refreshed = file_store.load_metadata(sid)
        assert refreshed is not None and refreshed.last_interaction is not None
        resumed = sm.resume_session(sid)
        assert [
            node.uuid for node in resumed.context_manager.get_context_window()
        ] == chain_before

    async def test_blocked_session_is_refused(
        self, root: Path, sm: SessionManager, file_store: FileSessionStore
    ) -> None:
        """被订阅的会话不可归档（钉住条件与 release 同一套）。"""
        session = sm.create_session()
        sid = session.session_id
        session.context_manager.add_message(Message(role="user", content="hello"))
        event_bus.route_attach("client-1", sid)
        try:
            with pytest.raises(RuntimeError) as excinfo:
                sm.archive_session(sid)
            assert "subscribed" in str(excinfo.value)
        finally:
            event_bus.route_detach("client-1", sid)
        # 拒绝发生在任何移动之前：live 目录还在、没进归档区、会话仍在内存。
        assert (root / sid).is_dir()
        assert file_store.is_archived(sid) is False
        assert sm.get_session(sid) is not None

    async def test_unknown_id_and_memory_backend(
        self, root: Path, sm: SessionManager
    ) -> None:
        with pytest.raises(LookupError):
            sm.archive_session("no-such-session")
        with pytest.raises(LookupError):
            sm.unarchive_session("no-such-session")

        memory_session = sm.create_session(backend="memory")
        with pytest.raises(RuntimeError) as excinfo:
            sm.archive_session(memory_session.session_id)
        assert "does not support archiving" in str(excinfo.value)

    async def test_create_or_adopt_on_archived_id_is_refused(
        self, root: Path, sm: SessionManager, file_store: FileSessionStore
    ) -> None:
        """create-or-adopt 撞上归档 id：报「已归档」，绝不新建同 id 的 live 会话。"""
        _seed_old(file_store, "team-a", days_ago=None)
        sm.archive_session("team-a")

        with pytest.raises(SessionArchivedError):
            sm.create_session(session_id="team-a")
        assert not (root / "team-a").exists()


class TestArchiveSweep:
    async def test_sweep_archives_only_stale_sessions(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        """只动「年龄已知且超过阈值」的会话；pin 豁免；未知年龄不动。"""
        _seed_old(file_store, "old", days_ago=90)
        _seed_old(file_store, "old-pinned", days_ago=90, tags=["pin"])
        _seed_old(file_store, "fresh", days_ago=0)
        _seed_old(file_store, CUSTOM_ID, days_ago=None)  # 自定义 id + 无时间
        sm = SessionManager({"file": FileSessionStore(root)})

        archived = await sm.archive_stale_sessions(idle_days=30.0)

        assert archived == ["old"]
        assert file_store.is_archived("old") is True
        assert not file_store.is_archived("old-pinned")
        assert not file_store.is_archived("fresh")
        assert not file_store.is_archived(CUSTOM_ID)

    async def test_sweep_disabled_by_non_positive_threshold(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        _seed_old(file_store, "old", days_ago=90)
        sm = SessionManager({"file": FileSessionStore(root)})

        assert await sm.archive_stale_sessions(idle_days=0.0) == []
        assert await sm.archive_stale_sessions(idle_days=-1.0) == []
        assert file_store.is_archived("old") is False

    async def test_final_check_invalidates_touched_candidate(
        self, root: Path, file_store: FileSessionStore, monkeypatch
    ) -> None:
        """阶段①到②之间被交互 → 当场失效（重读磁盘事实，不按旧快照执行）。

        用"冻结阶段①产物"的方式只测阶段②：直接把候选塞给 sweep，再改现场——
        不这样做的话候选会被阶段①按现场重新算一遍，终检永远走不到。
        """
        _seed_old(file_store, "old", days_ago=90)
        sm = SessionManager({"file": FileSessionStore(root)})
        candidates = sm._collect_stale(30.0)
        assert [session_id for _, session_id in candidates] == ["old"]
        monkeypatch.setattr(sm, "_collect_stale", lambda idle_days: candidates)

        # 终检前刚被交互：metadata 拨新 → 失效。
        file_store.save_metadata(
            "old",
            SessionMetadata(
                session_name="touched", last_interaction=datetime.now().isoformat()
            ),
        )
        assert await sm.archive_stale_sessions(idle_days=30.0) == []
        assert file_store.is_archived("old") is False

    async def test_final_check_invalidates_pinned_candidate(
        self, root: Path, file_store: FileSessionStore, monkeypatch
    ) -> None:
        """候选被补打 pin → 终检豁免（pin 豁免在阶段②也生效）。"""
        _seed_old(file_store, "old", days_ago=90)
        sm = SessionManager({"file": FileSessionStore(root)})
        candidates = sm._collect_stale(30.0)
        monkeypatch.setattr(sm, "_collect_stale", lambda idle_days: candidates)

        file_store.save_metadata(
            "old",
            SessionMetadata(
                session_name="pinned",
                last_interaction=datetime.now().isoformat(),
                tags=["pin"],
            ),
        )
        assert await sm.archive_stale_sessions(idle_days=30.0) == []
        assert file_store.is_archived("old") is False

    async def test_final_check_invalidates_disappeared_candidate(
        self, root: Path, file_store: FileSessionStore, monkeypatch
    ) -> None:
        """候选目录在终检前消失（手工删除）→ 跳过，不是报错。"""
        _seed_old(file_store, "old", days_ago=90)
        sm = SessionManager({"file": FileSessionStore(root)})
        candidates = sm._collect_stale(30.0)
        monkeypatch.setattr(sm, "_collect_stale", lambda idle_days: candidates)

        shutil.rmtree(root / "old")
        assert await sm.archive_stale_sessions(idle_days=30.0) == []

    async def test_sweep_leaves_in_memory_sessions_alone(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        """内存中的过期会话不动（sweep 只管不在工作集里的会话）。

        这是 S-2 的解法：resume 刻意不写 metadata（"空转 resume 零写盘"是既有
        不变量），所以"用户刚打开的旧会话"不能靠年龄刷新保护——靠"在内存"这条
        规则：前端持有 / 刚被打开的会话不为后台所动；逐出后重新是磁盘会话。
        """
        _seed_old(file_store, "old", days_ago=90)
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.resume_session("old")  # 换入内存
        assert sm.get_session(session.session_id) is not None

        assert await sm.archive_stale_sessions(idle_days=30.0) == []
        assert sm.get_session("old") is not None
        assert file_store.is_archived("old") is False

        # 逐出（release / 空闲回收 / 显式）之后它是磁盘会话，下一轮照常收走。
        sm.evict("old", reason="test")
        await sm.wait_teardowns()
        assert await sm.archive_stale_sessions(idle_days=30.0) == ["old"]
        assert file_store.is_archived("old") is True
        # 拆解不得把目录写回 live 区（数据分裂红线）。
        assert not (root / "old").exists()

    async def test_sweep_moves_a_disk_only_session_despite_subscribers(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        """订阅状态属于 live 区，磁盘会话的 sweep 不看它：过期即收走。"""
        _seed_old(file_store, "old", days_ago=90)
        sm = SessionManager({"file": FileSessionStore(root)})

        assert await sm.archive_stale_sessions(idle_days=30.0) == ["old"]
        assert file_store.is_archived("old") is True


class TestRestoredSessionSurvivesNextSweep:
    """恢复是显式兴趣信号：下一轮 sweep 不能按旧年龄把它立刻收回去（S1）。"""

    async def test_restore_is_sticky(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        _seed_old(file_store, "old", days_ago=90)
        sm = SessionManager({"file": FileSessionStore(root)})
        assert await sm.archive_stale_sessions(idle_days=30.0) == ["old"]

        assert sm.unarchive_session("old") == (True, "restored")

        # 阈值 30 天：恢复刚刷新过 last_interaction → 不再满足"年龄已知且超过
        # 阈值"，sweep 不得再收走它。
        assert await sm.archive_stale_sessions(idle_days=30.0) == []
        assert file_store.is_archived("old") is False


class TestEvictedSessionWriteSeal:
    """摘除即封写：evict 之后任何落盘重试不得复活 live 目录。

    现实触发链（CI review B1）：逐出时的同步 usage 落盘失败（ENOSPC / I/O
    错误）→ 脏标记保留（usage.flush 失败绝不抛出）→ 归档 rename → teardown
    的 aclose 收尾重试 → 写路径把 ``<root>/<sid>/`` 连父目录一起重建 →
    live 与归档区双存在（两侧 409，用户被锁死）。封盘让收尾重试成为 no-op。
    """

    async def test_failed_flush_retry_cannot_resurrect_the_live_dir(
        self, root: Path, sm: SessionManager, file_store: FileSessionStore, monkeypatch
    ) -> None:
        session = sm.create_session()
        sid = session.session_id
        session.context_manager.add_message(Message(role="user", content="hello"))

        # 注入"逐出时落盘失败"：usage.flush 吞掉异常但保留脏标记，等待下一个
        # 落盘点——那个落盘点（teardown 的 aclose）落在 rename 之后。
        real_save = file_store.save_metadata
        attempts: list[str] = []

        def flaky_save(session_id: str, metadata: SessionMetadata) -> None:
            attempts.append(session_id)
            raise OSError("disk full (injected)")

        monkeypatch.setattr(file_store, "save_metadata", flaky_save)
        session._usage.record_turn()  # 未落盘的增量（turn 收尾走同一条路）

        assert sm.archive_session(sid) == (True, "archived")

        # 磁盘"恢复"：若收尾重试还敢写，这里就会把 live 目录写回来。
        monkeypatch.setattr(file_store, "save_metadata", real_save)
        await sm.wait_teardowns()

        assert attempts == [sid], "只该发生逐出时那一次（失败的）落盘尝试"
        assert not (root / sid).exists(), "live 目录被收尾落盘写回来了（数据分裂红线）"
        assert file_store.is_archived(sid) is True
        assert (root / ".archive" / sid / "history.jsonl").exists()

    async def test_detached_session_log_appends_are_sealed(
        self, root: Path, sm: SessionManager, file_store: FileSessionStore
    ) -> None:
        """消息日志写口同样被封：摘除后 append 不得把 live 目录写回来。

        日志 append 是最"深"的一条写路径（worker 直接持有 ContextManager，
        不经 `_sessions` 查找）——封盘必须盖住它，否则它能把归档后的目录重建。
        """
        session = sm.create_session()
        sid = session.session_id
        session.context_manager.add_message(Message(role="user", content="hello"))
        assert (root / sid / "history.jsonl").exists()

        sm.evict(sid, reason="test")
        await sm.wait_teardowns()
        shutil.rmtree(root / sid)  # 模拟归档 rename 把目录移走

        session.context_manager.add_message(Message(role="user", content="late"))

        assert not (root / sid).exists(), "迟到的 append 把 live 目录写回来了"
        assert session.context_manager.get_context_window()  # 内存态照旧

    async def test_detached_session_persistence_is_sealed(
        self, root: Path, sm: SessionManager, file_store: FileSessionStore, monkeypatch
    ) -> None:
        """封盘本身：detach 之后 flush_usage / aclose 收尾都不再触达 store。"""
        session = sm.create_session()
        session.context_manager.add_message(Message(role="user", content="hello"))
        session.detach()
        session._usage.record_turn()  # 制造脏标记

        writes: list[str] = []
        monkeypatch.setattr(
            file_store,
            "save_metadata",
            lambda session_id, metadata: writes.append(session_id),
        )
        session.flush_usage()
        await session.aclose()

        assert writes == []


class TestResumedSessionIsProtectedWhileResident:
    """resume 打开的旧会话不被后台收走——保护来自「sweep 只看磁盘会话」，
    而不是年龄刷新（resume 刻意不写 metadata：空转 resume 零写盘是既有不变量）。"""

    async def test_resumed_old_session_survives_the_next_sweep(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        _seed_old(file_store, "old", days_ago=200)
        sm = SessionManager({"file": FileSessionStore(root)})

        sm.resume_session("old")  # 磁盘年龄仍是 200 天
        metadata = file_store.load_metadata("old")
        assert metadata is not None and metadata.last_interaction is not None

        # 阈值 90 天：年龄超阈值，但它在内存里 → sweep 不动它。
        assert await sm.archive_stale_sessions(idle_days=90.0) == []
        assert file_store.is_archived("old") is False
        assert sm.get_session("old") is not None

    async def test_resume_still_requires_an_existing_session(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        """resume 只对已存在的会话生效（不凭空建目录）——年龄刷新因此只发生在
        磁盘上真实存在的会话上，"resume 一个陌生 id 就落一个目录"不成立。"""
        sm = SessionManager({"file": FileSessionStore(root)})

        with pytest.raises(LookupError):
            sm.resume_session("brand-new-id")
        assert not (root / "brand-new-id").exists()


class TestSweepBatching:
    """B-1：批量 sweep 不得是事件循环上的同步长块，索引也不得逐条重写。"""

    async def test_sweep_yields_and_writes_the_index_once(
        self, root: Path, file_store: FileSessionStore, monkeypatch
    ) -> None:
        for i in range(30):
            _seed_old(file_store, f"old-{i:02d}", days_ago=90)
        sm = SessionManager({"file": file_store})

        writes = 0
        real_write = file_store._write_archive_index

        def counting_write(entries):
            nonlocal writes
            writes += 1
            real_write(entries)

        monkeypatch.setattr(file_store, "_write_archive_index", counting_write)

        # 心跳任务：只有 sweep 让出事件循环它才有机会跑（同步长块 = 停摆）。
        ticks = 0

        async def heartbeat() -> None:
            nonlocal ticks
            while True:
                ticks += 1
                await asyncio.sleep(0)

        hb = asyncio.create_task(heartbeat())
        try:
            archived = await sm.archive_stale_sessions(idle_days=30.0)
        finally:
            hb.cancel()

        assert len(archived) == 30
        assert ticks > 0, "sweep 必须让出事件循环（否则整批是一个同步长块）"
        assert writes == 1, f"索引应一次落盘（实际 {writes} 次 = O(n²) 写）"
        assert file_store.list_archived() and len(file_store.list_archived()) == 30


class TestArchivedList:
    async def test_entry_carries_tags_and_model(
        self, root: Path, sm: SessionManager
    ) -> None:
        session = sm.create_session(tags=["favorite"])
        sid = session.session_id
        sm.archive_session(sid)

        entry = sm.list_archived_sessions()[0]
        assert entry.id == sid
        assert entry.tags == ["favorite"]
        assert entry.archived_at is not None
        assert entry.model_id == session.model_id

    async def test_unknown_id_error_distinguishes_archive(
        self, root: Path, sm: SessionManager, file_store: FileSessionStore
    ) -> None:
        """missing_session_error：归档命中 → 可行动错误；真不存在 → 普通 LookupError。"""
        assert isinstance(sm.missing_session_error("nope"), LookupError)
        _seed_old(file_store, "old", days_ago=None)
        sm.archive_session("old")
        assert isinstance(sm.missing_session_error("old"), SessionArchivedError)

    async def test_archived_index_survives_restart(
        self, root: Path, file_store: FileSessionStore
    ) -> None:
        """新进程（新 SM）读归档列表：索引即可（不扫会话目录）。"""
        _seed_old(file_store, "old", days_ago=None)
        file_store.archive("old")
        fresh_sm = SessionManager({"file": FileSessionStore(root)})
        assert [e.id for e in fresh_sm.list_archived_sessions()] == ["old"]
        # 索引文件格式：{"sessions": [...]}（前端/工具可直接读）
        payload = json.loads(
            (root / ".archive" / "index.json").read_text(encoding="utf-8")
        )
        assert payload["sessions"][0]["id"] == "old"


class TestArchiverAndRuntimeEntryPoints:
    """BackgroundScheduler 实际注册的两个入口（archiver.sweep / runtime 转发）。"""

    async def test_archiver_reads_config_and_sweeps(
        self, root: Path, file_store: FileSessionStore, monkeypatch
    ) -> None:
        from types import SimpleNamespace

        from wing.session import SessionArchiver

        _seed_old(file_store, "old", days_ago=90)
        sm = SessionManager({"file": FileSessionStore(root)})
        archiver = SessionArchiver(sm)

        def fake_config(idle_days: float) -> SimpleNamespace:
            return SimpleNamespace(
                sessions=SimpleNamespace(archive=SimpleNamespace(idle_days=idle_days))
            )

        monkeypatch.setattr(
            "wing.session.archiver.get_config", lambda: fake_config(30.0)
        )
        assert await archiver.sweep() == ["old"]
        assert file_store.is_archived("old") is True

        # <= 0 = 关闭：再种一个也不扫（sweep 直接返回空，不触达 store）。
        _seed_old(file_store, "old-2", days_ago=90)
        monkeypatch.setattr(
            "wing.session.archiver.get_config", lambda: fake_config(0.0)
        )
        assert await archiver.sweep() == []
        assert file_store.is_archived("old-2") is False

    async def test_runtime_forwards_roundtrip_and_sweep(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        """WingRuntime 的四个转发方法（网关路由与 job 的实际入口）。"""
        monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
        from wing.runtime import WingRuntime

        runtime = WingRuntime()
        session = runtime.create_session()
        session_id = session.session_id

        # 空会话（从未落盘）：没有可归档的磁盘状态 → 幂等空动作，不是 500。
        assert runtime.archive_session(session_id) == (False, "nothing to archive")
        session.context_manager.add_message(Message(role="user", content="hi"))

        assert runtime.list_archived_sessions() == []
        assert runtime.is_archived(session_id) is False
        assert runtime.archive_session(session_id) == (True, "archived")
        assert [e.id for e in runtime.list_archived_sessions()] == [session_id]
        assert runtime.is_archived(session_id) is True  # 回执用的真实状态
        assert runtime.unarchive_session(session_id) == (True, "restored")
        assert runtime.is_archived(session_id) is False
        with pytest.raises(LookupError):
            runtime.archive_session("no-such-session")
        with pytest.raises(LookupError):
            runtime.unarchive_session("no-such-session")

        # missing_session_error：找不到时按归档区区分两种错误（路由的 404 用）。
        assert isinstance(runtime.missing_session_error("no-such-session"), LookupError)
        assert runtime.archive_session(session_id) == (True, "archived")
        assert isinstance(
            runtime.missing_session_error(session_id), SessionArchivedError
        )
        assert runtime.unarchive_session(session_id) == (True, "restored")

        # require_live_session：live 命中放行；归档命中抛结构化错误（media 预检用）。
        runtime.require_live_session(session_id)
        assert runtime.archive_session(session_id) == (True, "archived")
        with pytest.raises(SessionArchivedError):
            runtime.require_live_session(session_id)
        with pytest.raises(LookupError):
            runtime.require_live_session("no-such-session")
        assert runtime.unarchive_session(session_id) == (True, "restored")

        # job 入口：走 archiver → 当前配置（默认 90 天）→ 新会话不够老。
        assert await runtime.archive_stale_sessions() == []
