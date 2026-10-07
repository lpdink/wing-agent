"""create-or-adopt（`create_session(session_id=...)`）与 resume 覆盖子集。

被守的语义（``SessionManager.create_session`` / ``resume_session``）：

- **create-or-adopt**：给了 session_id——不存在则以该 id 建会话（id 即最终
  id）；已存在（内存或任 store）则收养既有会话（同 resume：模板 / workspace
  来自 metadata、不触发 ``before_session_start``）；
- **resume 覆盖子集**：只应用 model / provider / effort / tools；system_prompt /
  append_system_prompt / max_turns / yolo **一律不应用**（不改链上前缀 / 不改
  会话既有限额），且被忽略的字段要出声（warning 日志）；
- **零残留**：非法 id / 非法标签 / 无法解析的工具 ref 都必须在**任何写盘之前**
  失败——否则下一次同 id 的 create 会"收养"一个半成品幽灵会话。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wing.hooks import hooks
from wing.schema import Message
from wing.session import AgentOverride, SessionManager, tool_refs
from wing.store import FileSessionStore, MemorySessionStore

#: 编排方自带的 UUID（真实消费方形态；旧闸门会拒绝，新闸门接受）。
CUSTOM_ID = "3f2b9d1e-6c1a-4f2b-9d3e-1a2b3c4d5e6f"


@pytest.fixture
def root(tmp_path: Path) -> Path:
    return tmp_path / "sessions"


@pytest.fixture
def sm(root: Path) -> SessionManager:
    return SessionManager({"file": FileSessionStore(root)})


def _restart(root: Path) -> SessionManager:
    """新进程等价物：同一存储根上的新 SessionManager（内存态归零）。"""
    return SessionManager({"file": FileSessionStore(root)})


def _seed(session, *contents: str) -> None:
    """向会话上下文写入消息（不经 LLM）。"""
    for i, content in enumerate(contents):
        role = "user" if i % 2 == 0 else "assistant"
        session.context_manager.add_message(Message(role=role, content=content))


def _roles(session) -> list[str]:
    return [m.role for m in session.context_manager.get_context_window()]


def _history_lines(root: Path, session_id: str) -> list[dict]:
    path = root / session_id / "history.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _session_dirs(root: Path) -> list[str]:
    if not root.exists():
        return []
    return sorted(p.name for p in root.iterdir() if p.is_dir())


class TestCreateWithRequestedId:
    """不存在 → 以给定 id 建会话。"""

    @pytest.mark.asyncio
    async def test_id_is_exact_and_lands_on_disk(self, sm: SessionManager, root: Path):
        session = sm.create_session(session_id=CUSTOM_ID)
        assert session.session_id == CUSTOM_ID
        _seed(session, "hello")
        assert (root / CUSTOM_ID / "history.jsonl").exists()
        assert [s.id for s in sm.list_sessions()] == [CUSTOM_ID]

    @pytest.mark.asyncio
    async def test_absent_id_keeps_backend_generation(self, sm: SessionManager):
        """不给 session_id → 行为不变（自生成形态）。"""
        session = sm.create_session()
        assert session.session_id != CUSTOM_ID
        assert session.session_id[:8].isdigit()

    @pytest.mark.asyncio
    async def test_invalid_id_raises_and_leaves_no_trace(
        self, sm: SessionManager, root: Path
    ):
        """闸门先于一切：非法 id 不得触达 store，也不得留下任何文件。"""
        for bad in ("../escape", ".media", "a/b", "..", "\x00", "x" * 129):
            with pytest.raises(ValueError):
                sm.create_session(session_id=bad, tags=["favorite"])
        assert _session_dirs(root) == []
        assert not (root.parent / "escape").exists()

    @pytest.mark.asyncio
    async def test_invalid_tag_raises_before_any_write(
        self, sm: SessionManager, root: Path
    ):
        """非法标签在任何副作用之前失败 → 磁盘零痕迹（可安全重试同 id）。"""
        with pytest.raises(ValueError):
            sm.create_session(
                session_id=CUSTOM_ID,
                agent_override=AgentOverride(model="gpt-4o-mini"),
                tags=["bad tag"],
            )
        assert _session_dirs(root) == []

        # 重试（合法标签）是**新建**，不是收养半成品
        session = sm.create_session(session_id=CUSTOM_ID, tags=["favorite"])
        assert session.session_id == CUSTOM_ID
        assert session.tags == ["favorite"]
        _seed(session, "hi")
        assert len(_history_lines(root, CUSTOM_ID)) == 1

    @pytest.mark.asyncio
    async def test_unresolvable_tools_leave_no_trace(
        self, sm: SessionManager, root: Path
    ):
        with pytest.raises(ValueError):
            sm.create_session(
                session_id=CUSTOM_ID,
                agent_override=AgentOverride(model="gpt-4o-mini", tools=["Nope"]),
            )
        assert _session_dirs(root) == []


class TestAdoptExistingSession:
    """已存在 → 收养（同 resume 语义）。"""

    @pytest.mark.asyncio
    async def test_in_memory_adopt_returns_same_session(
        self, sm: SessionManager, root: Path
    ):
        created = sm.create_session(session_id=CUSTOM_ID)
        _seed(created, "hello")

        adopted = sm.create_session(session_id=CUSTOM_ID)

        assert adopted is created
        assert _session_dirs(root) == [CUSTOM_ID]  # 不产生第二个会话
        assert len(_history_lines(root, CUSTOM_ID)) == 1

    @pytest.mark.asyncio
    async def test_adopt_after_restart_hydrates_same_history(
        self, sm: SessionManager, root: Path
    ):
        created = sm.create_session(session_id=CUSTOM_ID)
        _seed(created, "hello", "world")

        restarted = _restart(root)
        adopted = restarted.create_session(session_id=CUSTOM_ID)

        assert adopted.session_id == CUSTOM_ID
        assert _roles(adopted) == ["user", "assistant"]
        assert _session_dirs(root) == [CUSTOM_ID]

    @pytest.mark.asyncio
    async def test_adopt_does_not_rerun_before_session_start(
        self, sm: SessionManager, monkeypatch: pytest.MonkeyPatch
    ):
        """收养不是"创建新 session"：hook 不重跑（前缀身份不因换入内存而漂移）。"""
        calls: list[str] = []
        original = hooks.invoke

        def recorder(name: str, *args, **kwargs):
            calls.append(name)
            return original(name, *args, **kwargs)

        monkeypatch.setattr(hooks, "invoke", recorder)

        sm.create_session(session_id=CUSTOM_ID)
        assert calls == ["before_session_start"]

        calls.clear()
        sm.create_session(session_id=CUSTOM_ID)  # 收养
        assert calls == []

        calls.clear()
        sm.create_session()  # 自生成 id 的新会话照旧触发
        assert calls == ["before_session_start"]

    @pytest.mark.asyncio
    async def test_adopt_applies_tags(self, sm: SessionManager):
        created = sm.create_session(session_id=CUSTOM_ID)
        adopted = sm.create_session(session_id=CUSTOM_ID, tags=["favorite", "task=x"])
        assert adopted is created
        assert adopted.tags == ["favorite", "task=x"]

    @pytest.mark.asyncio
    async def test_memory_backend_create_or_adopt(self):
        sm = SessionManager({"memory": MemorySessionStore()}, default_backend="memory")
        created = sm.create_session(session_id=CUSTOM_ID)
        _seed(created, "hello")
        adopted = sm.create_session(session_id=CUSTOM_ID)
        assert adopted is created
        assert sm.list_sessions()[0].id == CUSTOM_ID


class TestAdoptAppliesResumeSubset:
    """收养时的 agent 覆盖 = resume 子集（model/provider/effort/tools）。"""

    @pytest.mark.asyncio
    async def test_applied_fields_take_effect_and_persist(self, sm: SessionManager):
        created = sm.create_session(session_id=CUSTOM_ID)
        adopted = sm.create_session(
            session_id=CUSTOM_ID,
            agent_override=AgentOverride(
                model="gpt-4o-mini",
                effort="high",
                tools=["Read", "Bash"],
            ),
        )

        assert adopted is created
        assert adopted.agent.model == "gpt-4o-mini"
        assert adopted.agent.model_provider.reasoning_effort == "high"
        assert sorted(tool_refs(adopted.agent.tools)) == ["Bash", "Read"]

        metadata = adopted.store.load_metadata(CUSTOM_ID)
        assert metadata is not None
        assert metadata.model_name == "gpt-4o-mini"
        assert metadata.reasoning_effort == "high"
        assert sorted(metadata.tools or []) == ["Bash", "Read"]

    @pytest.mark.asyncio
    async def test_create_only_fields_are_ignored(self, sm: SessionManager):
        """system_prompt / append_system_prompt / max_turns / yolo 在收养时不生效。"""
        created = sm.create_session(session_id=CUSTOM_ID)
        before_prompt = created.context_manager.setin_system_prompt
        before_max_turns = created.agent.max_turns
        before_yolo = created.agent.yolo

        adopted = sm.create_session(
            session_id=CUSTOM_ID,
            agent_override=AgentOverride(
                system_prompt="SHOULD NOT APPLY",
                append_system_prompt="NEITHER",
                max_turns=1,
                yolo=True,
            ),
        )

        assert adopted.context_manager.setin_system_prompt == before_prompt
        assert adopted.context_manager.append_system_prompt == before_prompt
        assert adopted.agent.max_turns == before_max_turns
        assert adopted.agent.yolo is before_yolo
        # 未应用的字段不落任何记录（全字段 None → 甚至没有 metadata 文件）
        metadata = adopted.store.load_metadata(CUSTOM_ID)
        assert metadata is None or (
            metadata.system_prompt is None
            and metadata.append_system_prompt is None
            and metadata.max_turns is None
            and metadata.yolo is None
        )

    @pytest.mark.asyncio
    async def test_invalid_tools_do_not_apply_model(self, sm: SessionManager):
        """覆盖里的工具 ref 无法解析 → 在任何字段生效之前失败（无半截状态）。"""
        created = sm.create_session(session_id=CUSTOM_ID)
        before_model = created.agent.model

        with pytest.raises(ValueError):
            sm.create_session(
                session_id=CUSTOM_ID,
                agent_override=AgentOverride(model="gpt-4o-mini", tools=["Nope"]),
            )

        metadata = created.store.load_metadata(CUSTOM_ID)
        assert created.agent.model == before_model
        assert metadata is None or metadata.model_name is None


class TestResumeOverrideSubset:
    """`resume_session(session_id, agent_override=...)`：两条路径都应用子集。"""

    @pytest.mark.asyncio
    async def test_applies_to_in_memory_session(self, sm: SessionManager):
        session = sm.create_session()
        _seed(session, "hello")

        resumed = sm.resume_session(
            session.session_id,
            agent_override=AgentOverride(model="gpt-4o-mini", effort="low"),
        )

        assert resumed is session
        assert resumed.agent.model == "gpt-4o-mini"
        assert resumed.agent.model_provider.reasoning_effort == "low"
        assert _roles(resumed) == ["user"]

    @pytest.mark.asyncio
    async def test_applies_to_hydrated_session_and_persists(
        self, sm: SessionManager, root: Path
    ):
        session = sm.create_session()
        session_id = session.session_id
        _seed(session, "hello")

        restarted = _restart(root)
        resumed = restarted.resume_session(
            session_id, agent_override=AgentOverride(model="gpt-4o-mini")
        )
        assert resumed.agent.model == "gpt-4o-mini"
        assert _roles(resumed) == ["user"]

        # 覆盖随 metadata 落盘：第三次启动不带覆盖也还原同一模型。
        third = _restart(root)
        assert third.resume_session(session_id).agent.model == "gpt-4o-mini"

    @pytest.mark.asyncio
    async def test_not_found_still_raises_lookup_error(self, sm: SessionManager):
        with pytest.raises(LookupError):
            sm.resume_session("does-not-exist", agent_override=None)

    @pytest.mark.asyncio
    async def test_invalid_tools_do_not_apply_model(self, sm: SessionManager):
        session = sm.create_session()
        before_model = session.agent.model
        with pytest.raises(ValueError):
            sm.resume_session(
                session.session_id,
                agent_override=AgentOverride(model="gpt-4o-mini", tools=["Nope"]),
            )
        assert session.agent.model == before_model
