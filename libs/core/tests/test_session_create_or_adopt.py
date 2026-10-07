"""create-or-adopt（`create_session(session_id=...)`）与 resume 覆盖子集。

被守的语义（``SessionManager.create_session`` / ``resume_session``）：

- **create-or-adopt**：给了 session_id——不存在则以该 id 建会话（id 即最终
  id）；已存在（内存或任 store）则收养既有会话（同 resume：模板 / workspace
  来自 metadata、不触发 ``before_session_start``）；
- **resume 覆盖子集**：只应用 model / provider / effort / tools；system_prompt /
  append_system_prompt / max_turns / yolo **一律不应用**（不改链上前缀 / 不改
  会话既有限额），且被忽略的字段要出声（warning 日志）；
- **零残留**：非法 id / 非法标签 / 无法解析的工具 ref 都必须在**任何写盘之前**
  失败——否则下一次同 id 的 create 会"收养"一个半成品幽灵会话；
- **别名不得静默混合**：大小写 / Unicode 归一化不敏感的文件系统上，请求 id 的
  变体会解析到同一份日志——内存键必须等于**存储键**，绝不允许两个 Session
  共用一份 ``history.jsonl``。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wing.hooks import hooks
from wing.schema import Message
from wing.session import AgentOverride, SessionManager, tool_refs
from wing.session.override import non_utf8_override_fields
from wing.session.session import ignored_override_fields
from wing.store import (
    FileSessionStore,
    MemorySessionStore,
)

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


class _CaseInsensitiveFileStore(FileSessionStore):
    """`resolve_stored_id` 大小写不敏感：模拟 macOS APFS / Windows NTFS 的别名。

    存储本身保持**逐字**：磁盘上真实存在的目录名就是首次写入者的拼写（真实不敏感
    FS 上 ``mkdir("Team-A")`` 之后 ``team-a`` 解析到同一目录，目录项名字仍是
    ``Team-A``）。真实不敏感 FS 上本类与父类等价（父类的 ``samefile`` 探测已能看见
    真名）；在大小写敏感的 Linux 上它把"别名"这一现象**造出来**，让红线契约在任何
    平台都可测——这正是 S1 想要的复现能力。
    """

    def resolve_stored_id(self, session_id: str) -> str | None:
        exact = super().resolve_stored_id(session_id)
        if exact is not None:
            return exact
        folded = session_id.casefold()
        if not self.root.is_dir():
            return None
        for entry in sorted(self.root.iterdir()):
            if entry.is_dir() and entry.name.casefold() == folded:
                return entry.name
        return None


def _fs_is_case_insensitive(base: Path) -> bool:
    """本机文件系统对大小写是否不敏感（决定别名是否真的发生）。"""
    probe = base / "fs-case-probe"
    probe.mkdir(parents=True, exist_ok=True)
    try:
        return (base / "FS-CASE-PROBE").is_dir()
    finally:
        probe.rmdir()


class TestFilesystemAliases:
    """大小写 / 归一化别名：内存键必须等于存储键（S1 红线）。"""

    @pytest.fixture
    def aliasing(self, tmp_path: Path) -> SessionManager:
        """别名 store（模拟 macOS / Windows 的不敏感 FS），任何平台都能跑。"""
        return SessionManager(
            {"file": _CaseInsensitiveFileStore(tmp_path / "sessions")}
        )

    @pytest.mark.asyncio
    async def test_case_variant_adopts_the_same_session(self, aliasing: SessionManager):
        created = aliasing.create_session(session_id="Team-A", tags=["x"])
        _seed(created, "from A")
        adopted = aliasing.create_session(session_id="team-a", tags=["y"])

        assert adopted is created
        assert adopted.session_id == created.session_id  # 真名，不是请求字符串
        assert list(aliasing._sessions) == [created.session_id]
        assert adopted.tags == ["x", "y"]

    @pytest.mark.asyncio
    async def test_alias_resolves_from_disk_too(self, tmp_path: Path):
        """冷路径（不在内存）：变体 resume 归一到同一会话，不得再水合一份。"""
        store = _CaseInsensitiveFileStore(tmp_path / "sessions")
        first = SessionManager({"file": store})
        created = first.create_session(session_id="Team-A", tags=["x"])
        _seed(created, "hello")

        restarted = SessionManager({"file": store})
        resumed = restarted.resume_session("team-a")
        assert resumed.session_id == "Team-A"
        assert _roles(resumed) == ["user"]
        assert list(restarted._sessions) == ["Team-A"]

    @pytest.mark.asyncio
    async def test_one_directory_never_gets_two_writers(
        self, aliasing: SessionManager, root: Path
    ):
        """两个变体 id 只对应**一个** Session 对象——一份日志只有一个写入者。"""
        a = aliasing.create_session(session_id="Team-A", tags=["x"])
        b = aliasing.create_session(session_id="team-a", tags=["y"])
        c = aliasing.create_session(session_id="TEAM-a", tags=["z"])
        assert a is b is c
        _seed(a, "only")
        assert len(_history_lines(root, "Team-A")) == 1

    @pytest.mark.asyncio
    async def test_empty_alias_converges_before_any_write(
        self, aliasing: SessionManager, root: Path
    ):
        """两个**空**会话（都还没落盘）也必须归一。

        这是别名最隐蔽的一档：没有 metadata / history，`exists` 不认它，只有
        "认领键"（mkdir 判据）能看见——不归一的话，两个 Session 的第一次写入会
        落进同一个目录（静默混合）。
        """
        a = aliasing.create_session(session_id="Team-E")  # 无 tags：只有认领
        b = aliasing.create_session(session_id="team-e")
        assert a is b
        assert list(aliasing._sessions) == ["Team-E"]
        _seed(a, "single writer")
        assert len(_history_lines(root, "Team-E")) == 1

    @pytest.mark.asyncio
    async def test_new_variant_still_creates(self, aliasing: SessionManager):
        """别名只在**真的命中**时归一：全新 id 照常新建（不得误伤）。"""
        created = aliasing.create_session(session_id="Team-A")
        fresh = aliasing.create_session(session_id="Team-B")
        assert created is not fresh
        assert sorted(aliasing._sessions) == ["Team-A", "Team-B"]

    @pytest.mark.asyncio
    async def test_alias_logs_a_warning(
        self, aliasing: SessionManager, wing_warnings: list[str]
    ):
        aliasing.create_session(session_id="Team-A")
        aliasing.create_session(session_id="team-a")
        assert any("resolves to existing session" in line for line in wing_warnings), (
            wing_warnings
        )

    @pytest.mark.asyncio
    async def test_real_filesystem_never_shares_one_history(self, tmp_path: Path):
        """真实 FS 上的不变量：变体 id 要么同一会话，要么**两个目录**——绝不混写。

        大小写敏感（Linux）时两个 id 是独立会话；不敏感（macOS/Windows）时归一到
        真名。两种结果都合法，唯一非法的是"两个会话共用一份 history.jsonl"。
        """
        root = tmp_path / "sessions"
        sm = SessionManager({"file": FileSessionStore(root)})
        aliases = _fs_is_case_insensitive(tmp_path)

        created = sm.create_session(session_id="Team-R", tags=["x"])
        second = sm.create_session(session_id="team-r", tags=["y"])

        if aliases:
            assert second is created
            assert second.session_id == "Team-R"
        else:
            assert second is not created
            assert second.session_id == "team-r"

        # 不变量（与 FS 类型无关）：回报的 id 逐字对应一个目录，目录数 == 会话数
        assert sorted(sm._sessions) == sorted({created.session_id, second.session_id})
        assert sorted(p.name for p in root.iterdir()) == sorted(sm._sessions)
        for session_id in sm._sessions:
            assert (root / session_id).is_dir(), session_id


class TestIgnoredOverrideWarnings:
    """被忽略的覆盖字段必须出声（N1/S3；``caplog`` 抓不到 wing logger）。"""

    @pytest.mark.asyncio
    async def test_resume_override_warns_about_ignored_fields(
        self, sm: SessionManager, wing_warnings: list[str]
    ):
        session = sm.create_session()
        sm.resume_session(
            session.session_id,
            agent_override=AgentOverride(
                model="gpt-4o-mini",
                system_prompt="X",
                append_system_prompt="Y",
                max_turns=1,
                yolo=True,
            ),
        )
        text = "\n".join(wing_warnings)
        assert "resume override ignores" in text, wing_warnings
        for field in ("system_prompt", "append_system_prompt", "max_turns", "yolo"):
            assert field in text, wing_warnings
        # 生效的字段不进 warning。
        assert "resume override ignores model" not in text

    @pytest.mark.asyncio
    async def test_provider_without_model_warns_on_resume(
        self, sm: SessionManager, wing_warnings: list[str]
    ):
        """`provider` 单独给出（没有 model）是 no-op——本步新增的一条静默路径。"""
        session = sm.create_session()
        sm.resume_session(
            session.session_id, agent_override=AgentOverride(provider="alt")
        )
        text = "\n".join(wing_warnings)
        assert "resume override ignores provider" in text, wing_warnings

    @pytest.mark.asyncio
    async def test_provider_without_model_warns_on_adopt(
        self, sm: SessionManager, wing_warnings: list[str]
    ):
        created = sm.create_session(session_id=CUSTOM_ID)
        adopted = sm.create_session(
            session_id=CUSTOM_ID, agent_override=AgentOverride(provider="alt")
        )
        assert adopted is created
        assert any("resume override ignores provider" in line for line in wing_warnings)

    @pytest.mark.asyncio
    async def test_provider_without_model_warns_on_create(
        self, sm: SessionManager, wing_warnings: list[str]
    ):
        """创建路径同一口径（provider 与 model 成对；``session/update`` 直接 400）。"""
        sm.create_session(agent_override=AgentOverride(provider="alt"))
        assert any("agent override ignores provider" in line for line in wing_warnings)

    @pytest.mark.asyncio
    async def test_provider_with_model_is_silent(
        self, sm: SessionManager, wing_warnings: list[str]
    ):
        session = sm.create_session()
        sm.resume_session(
            session.session_id,
            agent_override=AgentOverride(model="gpt-4o-mini", provider="alt"),
        )
        assert session.agent.model_provider.name == "alt"
        assert not [line for line in wing_warnings if "provider" in line], wing_warnings

    def test_ignored_override_fields_is_pure(self):
        """纯函数直接对账（warning 的判据，不依赖日志捕获）。"""
        assert ignored_override_fields(AgentOverride(), resume=True) == []
        assert ignored_override_fields(AgentOverride(provider="p"), resume=False) == [
            "provider"
        ]
        assert (
            ignored_override_fields(AgentOverride(model="m", provider="p"), resume=True)
            == []
        )
        assert ignored_override_fields(
            AgentOverride(provider="p", system_prompt="s", yolo=True),
            resume=True,
        ) == ["provider", "system_prompt", "yolo"]
        # 创建语义下只有 provider 的成对性构成"被忽略"。
        assert (
            ignored_override_fields(
                AgentOverride(system_prompt="s", max_turns=1), resume=False
            )
            == []
        )


class TestAdoptIgnoresCreateParams:
    """收养路径忽略创建参数（N5：同一请求体因 id 是否存在回报不同类别结果）。"""

    @pytest.mark.asyncio
    async def test_existing_id_skips_create_param_validation(self, sm: SessionManager):
        created = sm.create_session(session_id=CUSTOM_ID)
        adopted = sm.create_session(
            session_id=CUSTOM_ID,
            backend="nope",
            template_name="nope",
            workspace="/nope",
        )
        assert adopted is created
        assert adopted.template_name == created.template_name

    @pytest.mark.asyncio
    async def test_same_body_with_fresh_id_raises(self, sm: SessionManager, root: Path):
        with pytest.raises(ValueError):
            sm.create_session(
                session_id="brand-new", backend="nope", template_name="nope"
            )
        assert _session_dirs(root) == []


class TestNonUtf8InputGates:
    """非 UTF-8 输入（孤立代理字符）的闸门：400 级错误 + 零残留 + 无幽灵。

    "合法 JSON ≠ 合法 UTF-8"：``"\\ud800"`` 能过 JSON 解析，却过不了任何
    ``encode("utf-8")``——落盘（metadata / history）与出网序列化都会炸。旧行为是
    500 + 半份 metadata，随后同 id 的 create 会"收养"这个半成品幽灵会话。
    """

    def test_pure_field_scan(self):
        assert non_utf8_override_fields(AgentOverride()) == []
        assert non_utf8_override_fields(AgentOverride(model="m", effort="high")) == []
        assert non_utf8_override_fields(AgentOverride(system_prompt="\ud800")) == [
            "system_prompt"
        ]
        assert non_utf8_override_fields(
            AgentOverride(model="\ud800", provider="\udc00", tools=["ok", "\ud800"])
        ) == ["model", "provider", "tools[1]"]

    @pytest.mark.parametrize(
        "override",
        [
            AgentOverride(system_prompt="\ud800"),
            AgentOverride(append_system_prompt="\ud800"),
            AgentOverride(model="\ud800"),
            AgentOverride(provider="\ud800"),
            AgentOverride(effort="\ud800"),
            AgentOverride(tools=["\ud800"]),
        ],
    )
    @pytest.mark.asyncio
    async def test_create_rejects_and_leaves_no_trace(
        self, sm: SessionManager, root: Path, override: AgentOverride
    ):
        with pytest.raises(ValueError) as failure:
            sm.create_session(session_id="Ghost-1", agent_override=override)
        assert "UTF-8" in str(failure.value), failure.value
        # 连认领键都没发生（校验在它之前）→ 磁盘上没有任何痕迹
        assert _session_dirs(root) == []

    @pytest.mark.asyncio
    async def test_failed_create_does_not_leave_a_ghost(
        self, sm: SessionManager, root: Path
    ):
        """失败之后同 id 的 create 是**新建**：模型记录不存在、历史为空。"""
        with pytest.raises(ValueError):
            sm.create_session(
                session_id="Ghost-2",
                agent_override=AgentOverride(
                    model="gpt-4o-mini", system_prompt="\ud800"
                ),
            )
        session = sm.create_session(session_id="Ghost-2")
        assert session.session_id == "Ghost-2"
        # 幽灵的痕迹（模板默认提示词之外没有任何覆盖）不存在
        assert not session.context_manager.setin_system_prompt
        metadata = session.store.load_metadata("Ghost-2")
        assert metadata is None or metadata.model_name is None

    @pytest.mark.asyncio
    async def test_other_input_fields_are_gated_too(
        self, sm: SessionManager, root: Path
    ):
        """同一族的其它输入（workspace / tags）：合法性判定同源。"""
        with pytest.raises(ValueError):
            sm.create_session(session_id="H-1", workspace="/tmp/\ud800")
        with pytest.raises(ValueError):
            sm.create_session(session_id="H-2", tags=["\ud800"])
        assert _session_dirs(root) == []

    @pytest.mark.asyncio
    async def test_resume_and_update_gates_leave_state_untouched(
        self, sm: SessionManager
    ):
        session = sm.create_session(session_id="K-1")
        before = session.agent.model

        with pytest.raises(ValueError):
            sm.resume_session(
                "K-1",
                agent_override=AgentOverride(model="gpt-4o-mini", provider="\ud800"),
            )
        with pytest.raises(ValueError):
            session.set_title("\ud800")
        with pytest.raises(ValueError):
            session.set_workspace("/tmp/\ud800")
        with pytest.raises(ValueError):
            sm.set_session_tags("K-1", add=["\ud800"])
        with pytest.raises(ValueError):
            await session.post("\ud800")

        assert session.agent.model == before  # 没有半截状态
        metadata = session.store.load_metadata("K-1")
        assert metadata is None or (
            metadata.model_name is None and metadata.session_name is None
        )
