"""model_id 贯穿会话运行期：身份三元组 / 恢复链 / 模板与 reload / override。

覆盖（Step 02 的会话侧行为契约）：

- **身份三元组**：`update_state(model_id)` 切换并一次落盘 `(model_id, provider,
  name)`；新会话从模板反查补全 id（第一帧就有引用词）；
- **未知 model_id** → ValueError（C7 文案：available ids + 调用名提示），状态零变化；
- **恢复链三级**：① id 命中 → 用当前映射（跟随配置演化）② id 未命中 → 用快照 +
  `identify` 反查补 id + warning ③ 快照 provider 不可解析 → 回落模板默认（记录保留）；
- **旧记录**（无 model_id）→ 快照 + 反查补 id + 落盘对齐；**空转 resume 零写入**
  （一次性迁移，不是写噪声）；
- **模板**：switch_template 的 id（显式优先 / identify 补全）；fork 快照带 id；
- **reload_templates**：reload 后新会话与 resume 用同一新映射（不分叉）；
- **AgentOverride** 用 model_id；legacy 字段（model/provider）静默忽略。
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from wing.config import AgentConfig, Config, ModelSpec, ProviderConfig
from wing.schema import Message
from wing.session import AgentOverride, AgentTemplate, SessionManager
from wing.store import FileSessionStore, SessionMetadata


def _config(
    models: list[str | ModelSpec],
    *,
    agent_model: str,
    provider_name: str = "p",
) -> Config:
    """单 provider 测试配置（模板默认模型 = agent_model，必须是合法 id）。"""
    return Config(
        providers=[
            ProviderConfig(
                name=provider_name,
                base_url="http://x",
                api_key="k",
                models=models,
            )
        ],
        agents=[AgentConfig(name="default", model=agent_model)],
    )


def _use_config(monkeypatch: pytest.MonkeyPatch, cfg: Config) -> None:
    """替换配置单例（与 conftest 的 autouse fixture 同一入口）。"""
    import wing.config.loader as loader

    monkeypatch.setattr(loader, "_config", cfg)


def _seed(session: Any, *contents: str) -> None:
    for i, content in enumerate(contents):
        role = "user" if i % 2 == 0 else "assistant"
        session.context_manager.add_message(Message(role=role, content=content))


class _RecordingStore(FileSessionStore):
    """记录 `save_metadata` 调用的文件后端（落盘对齐的写噪声探针）。"""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.saves: list[str] = []

    def save_metadata(self, session_id: str, metadata: SessionMetadata) -> None:
        self.saves.append(session_id)
        super().save_metadata(session_id, metadata)


class TestIdentityTriple:
    """身份三元组的写点：切换 / 新建兜底 / 未动作不写记录。"""

    @pytest.mark.asyncio
    async def test_new_session_derives_model_id_from_template(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """无记录的新会话：id 由 live agent 反查补全（id 缺省 = 调用名）。"""
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="gpt-id", name="gpt-4")], agent_model="gpt-id"),
        )
        root = tmp_path / "sessions"
        sm = SessionManager({"file": FileSessionStore(root)})

        session = sm.create_session()

        assert session.model_id == "gpt-id"
        assert session.agent.model == "gpt-4"
        assert session.agent.provider_name == "p"
        # 未显式动作不写模型记录
        meta = FileSessionStore(root).load_metadata(session.session_id)
        assert meta is None or meta.model_id is None

    @pytest.mark.asyncio
    async def test_update_sets_triple_and_persists(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            _config(
                [
                    "gpt-4",
                    ModelSpec(
                        id="ds-flash", name="deepseek-flash", display_name="DS Flash"
                    ),
                ],
                agent_model="gpt-4",
            ),
        )
        root = tmp_path / "sessions"
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id

        await session.update_state(model_id="ds-flash")

        assert session.model_id == "ds-flash"
        assert session.agent.model == "deepseek-flash"
        assert session.agent.provider_name == "p"
        assert session.agent.model_display_name == "DS Flash"
        assert session.to_agent_info().model_id == "ds-flash"

        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is not None
        assert (meta.model_id, meta.model_name, meta.provider_name) == (
            "ds-flash",
            "deepseek-flash",
            "p",
        )

    @pytest.mark.asyncio
    async def test_unknown_model_id_raises_c7_and_changes_nothing(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """未命中 = 错误（C7 文案）；先查后改，内存态与磁盘都零变化。"""
        cfg = _config(
            [ModelSpec(id="ds-flash", name="deepseek-flash")], agent_model="ds-flash"
        )
        _use_config(monkeypatch, cfg)
        root = tmp_path / "sessions"
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id

        with pytest.raises(ValueError) as failure:
            await session.update_state(model_id="nope")

        text = str(failure.value)
        assert "unknown model id 'nope'" in text
        assert "available ids: ds-flash" in text
        assert session.model_id == "ds-flash"
        assert session.agent.model == "deepseek-flash"
        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is None or meta.model_id is None

    @pytest.mark.asyncio
    async def test_call_name_hint_points_at_the_id(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """把调用名当 id 发送（前端迁移期的典型误用）：错误提示写明它属于哪个 id。"""
        _use_config(
            monkeypatch,
            _config(
                [ModelSpec(id="ds-flash", name="deepseek-flash")],
                agent_model="ds-flash",
            ),
        )
        sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        session = sm.create_session()

        with pytest.raises(ValueError) as failure:
            await session.update_state(model_id="deepseek-flash")

        text = str(failure.value)
        assert "note: 'deepseek-flash' is the call name of model id 'ds-flash'" in text
        assert "provider 'p'" in text


class TestRestoreChain:
    """恢复链三级 + 旧记录迁移（每一步都有明确出口，无猜测）。"""

    @pytest.mark.asyncio
    async def test_id_hit_uses_current_mapping(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """① id 命中：用当前映射（id 不变、name/provider 变了跟随配置演化）。"""
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="shared", name="old-name")], agent_model="shared"),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id
        await session.update_state(model_id="shared")
        assert session.agent.model == "old-name"

        # 配置演化：同一 id 换调用名与 provider
        _use_config(
            monkeypatch,
            _config(
                [ModelSpec(id="shared", name="new-name")],
                agent_model="shared",
                provider_name="q",
            ),
        )
        restored = SessionManager({"file": FileSessionStore(root)}).resume_session(sid)

        assert restored.model_id == "shared"
        assert restored.agent.model == "new-name"
        assert restored.agent.provider_name == "q"
        # 快照跟随当前映射落盘（一次性对齐）
        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is not None
        assert (meta.model_id, meta.model_name, meta.provider_name) == (
            "shared",
            "new-name",
            "q",
        )

    @pytest.mark.asyncio
    async def test_deleted_id_falls_back_to_snapshot_and_backfills(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, wing_warnings: list[str]
    ):
        """② id 被删（调用名仍在）：快照继续跑 + 反查补 id + warning。"""
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="old-id", name="m")], agent_model="old-id"),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id
        await session.update_state(model_id="old-id")

        _use_config(
            monkeypatch,
            _config([ModelSpec(id="new-id", name="m")], agent_model="new-id"),
        )
        restored = SessionManager({"file": FileSessionStore(root)}).resume_session(sid)

        assert restored.agent.model == "m"
        assert restored.model_id == "new-id"  # identify 反查补全
        assert any("is not in config" in m for m in wing_warnings), wing_warnings
        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is not None
        assert meta.model_id == "new-id"
        assert (meta.model_name, meta.provider_name) == ("m", "p")

    @pytest.mark.asyncio
    async def test_deleted_id_with_undeclared_name_keeps_record(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, wing_warnings: list[str]
    ):
        """② 的退化档：调用名也不在声明里 → 不编 id（None），记录原样保留。"""
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="old-id", name="m")], agent_model="old-id"),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id
        await session.update_state(model_id="old-id")

        _use_config(
            monkeypatch,
            _config([ModelSpec(name="other")], agent_model="other"),
        )
        restored = SessionManager({"file": FileSessionStore(root)}).resume_session(sid)

        assert restored.agent.model == "m"  # 快照继续跑
        assert restored.model_id is None  # 反查查不中 → 不猜
        assert any("not declared by provider" in m for m in wing_warnings), (
            wing_warnings
        )
        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is not None
        # 旧 id 与快照是唯一恢复线索：不擦除（与第 3 级的「记录保留」同口径）
        assert (meta.model_id, meta.model_name, meta.provider_name) == (
            "old-id",
            "m",
            "p",
        )

    @pytest.mark.asyncio
    async def test_unresolvable_provider_falls_back_to_template(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, wing_warnings: list[str]
    ):
        """③ 快照 provider 不可解析（id 也未命中）→ 模板默认 + warning（记录保留）。"""
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="old-id", name="m")], agent_model="old-id"),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id
        await session.update_state(model_id="old-id")

        _use_config(
            monkeypatch,
            _config(
                [ModelSpec(id="other-id", name="other")],
                agent_model="other-id",
                provider_name="q",
            ),
        )
        restored = SessionManager({"file": FileSessionStore(root)}).resume_session(sid)

        assert restored.agent.model == "other"  # 模板默认
        assert restored.model_id == "other-id"
        assert any("cannot restore model" in m for m in wing_warnings), wing_warnings
        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is not None
        assert (meta.model_id, meta.model_name, meta.provider_name) == (
            "old-id",
            "m",
            "p",
        )

    @pytest.mark.asyncio
    async def test_legacy_record_backfills_id_and_aligns(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """④ 旧记录（无 model_id）：快照 + identify 反查补 id + 落盘对齐。"""
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="the-id", name="m")], agent_model="the-id"),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id
        # 模拟 id 字段引入前写下的记录
        session.store.save_metadata(
            sid, SessionMetadata(model_name="m", provider_name="p")
        )

        store = _RecordingStore(root)
        restored = SessionManager({"file": store}).resume_session(sid)

        assert restored.agent.model == "m"
        assert restored.model_id == "the-id"
        meta = store.load_metadata(sid)
        assert meta is not None
        assert meta.model_id == "the-id"
        assert store.saves == [sid]  # 一次性迁移：写了一次

        # 第二次启动（已对齐）：零写入
        store2 = _RecordingStore(root)
        SessionManager({"file": store2}).resume_session(sid)
        assert store2.saves == []

    @pytest.mark.asyncio
    async def test_untouched_resume_writes_nothing(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """空转 resume（配置未变）不产生任何 metadata 写（避免写噪声）。"""
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="the-id", name="m")], agent_model="the-id"),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id
        await session.update_state(model_id="the-id")

        store = _RecordingStore(root)
        restored = SessionManager({"file": store}).resume_session(sid)

        assert restored.model_id == "the-id"
        assert restored.agent.model == "m"
        assert store.saves == []


class TestTemplateModelId:
    """模板切换的引用词：显式 id 优先，缺省 identify 反查补全。"""

    @pytest.mark.asyncio
    async def test_switch_template_backfills_id(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config(
                [ModelSpec(id="tpl-id", name="tpl-name"), "gpt-4"],
                agent_model="gpt-4",
            ),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id

        template = AgentTemplate(name="coder", model="tpl-name", provider_name="p")
        await session.switch_template(template)

        assert session.model_id == "tpl-id"
        assert session.agent.model == "tpl-name"
        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is not None
        assert meta.model_id == "tpl-id"
        assert meta.template_name == "coder"

    @pytest.mark.asyncio
    async def test_switch_template_prefers_explicit_id(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """模板自带 id（fork 的 from_agent 形态）时不反查、直接采用。"""
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="tpl-id", name="tpl-name")], agent_model="tpl-id"),
        )
        sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        session = sm.create_session()

        template = AgentTemplate(
            name="coder",
            model="unmapped-name",
            provider_name="p",
            model_id="explicit-id",
        )
        await session.switch_template(template)

        assert session.model_id == "explicit-id"
        assert session.agent.model == "unmapped-name"

    @pytest.mark.asyncio
    async def test_from_agent_carries_model_id(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="src-id", name="src-name")], agent_model="src-id"),
        )
        sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        session = sm.create_session()

        assert (
            AgentTemplate.from_agent(session.agent, model_id="src-id").model_id
            == "src-id"
        )
        assert AgentTemplate.from_agent(session.agent).model_id is None


class TestForkSnapshot:
    """fork 的子会话记录源会话的引用词（快照语义）。"""

    @pytest.mark.asyncio
    async def test_fork_snapshots_model_id(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config(
                [ModelSpec(id="src-id", name="src-name"), "gpt-4"],
                agent_model="src-id",
            ),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        source = sm.create_session()
        _seed(source, "hello", "question")
        target = source.context_manager.get_context_window()[-1].uuid
        assert target is not None

        result = sm.fork_session(source.session_id, target)
        assert result is not None
        child, _ = result
        assert child.model_id == "src-id"

        meta = FileSessionStore(root).load_metadata(child.session_id)
        assert meta is not None
        assert meta.model_id == "src-id"
        assert (meta.model_name, meta.provider_name) == ("src-name", "p")

        # 子会话重启：id 优先，与源会话一致
        restored = SessionManager({"file": FileSessionStore(root)}).resume_session(
            child.session_id
        )
        assert restored.model_id == "src-id"
        assert restored.agent.model == "src-name"


class TestReloadTemplates:
    """reload 重建模板管理器：新会话与 resume 用同一新映射（不分叉）。"""

    @pytest.mark.asyncio
    async def test_reload_follows_new_mapping_on_both_paths(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        from wing.runtime import WingRuntime

        monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
        _use_config(
            monkeypatch,
            _config([ModelSpec(id="shared", name="old-name")], agent_model="shared"),
        )
        runtime = WingRuntime()
        session = runtime.create_session()
        sid = session.session_id
        await session.update_state(model_id="shared")
        assert session.agent.model == "old-name"

        # 配置演化：同一 id 换调用名；reload 让模板管理器也跟随
        cfg2 = _config([ModelSpec(id="shared", name="new-name")], agent_model="shared")
        _use_config(monkeypatch, cfg2)
        monkeypatch.setattr("wing.config.load_config", lambda reload=False: cfg2)

        result = await runtime.reload_system()
        assert result.ok, [i.name for i in result.items if not i.ok]

        # 新会话：模板来自新配置
        fresh = runtime.create_session()
        assert fresh.agent.model == "new-name"
        assert fresh.model_id == "shared"

        # resume：id 优先 → 同一新映射（两条路径不分叉）
        runtime.sm.evict(sid, reason="test")
        await runtime.sm.wait_teardowns()
        resumed = runtime.resume_session(sid)
        assert resumed.agent.model == "new-name"
        assert resumed.model_id == "shared"
        meta = FileSessionStore(tmp_path / "sessions").load_metadata(sid)
        assert meta is not None
        assert (meta.model_id, meta.model_name) == ("shared", "new-name")


class TestOverrideModelId:
    """AgentOverride 的模型字段 = model_id（legacy 字段静默忽略）。"""

    @pytest.mark.asyncio
    async def test_create_override_uses_model_id(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            _config(
                [ModelSpec(id="ds-flash", name="deepseek-flash"), "gpt-4"],
                agent_model="gpt-4",
            ),
        )
        sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})

        session = sm.create_session(agent_override=AgentOverride(model_id="ds-flash"))

        assert session.model_id == "ds-flash"
        assert session.agent.model == "deepseek-flash"
        meta = session.store.load_metadata(session.session_id)
        assert meta is not None
        assert (meta.model_id, meta.model_name) == ("ds-flash", "deepseek-flash")

    @pytest.mark.asyncio
    async def test_resume_override_uses_model_id(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            _config(
                [ModelSpec(id="ds-flash", name="deepseek-flash"), "gpt-4"],
                agent_model="gpt-4",
            ),
        )
        sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        session = sm.create_session()

        resumed = sm.resume_session(
            session.session_id, agent_override=AgentOverride(model_id="ds-flash")
        )

        assert resumed is session
        assert resumed.model_id == "ds-flash"
        assert resumed.agent.model == "deepseek-flash"

    @pytest.mark.asyncio
    async def test_legacy_override_fields_are_ignored(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            _config(
                [ModelSpec(id="ds-flash", name="deepseek-flash")],
                agent_model="ds-flash",
            ),
        )
        sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})

        override = AgentOverride.model_validate(
            {"model": "deepseek-flash", "provider": "p"}
        )
        assert override.model_id is None

        session = sm.create_session(agent_override=override)
        assert session.model_id == "ds-flash"  # 模板默认，未被旧字段影响


class TestNoPartialApplication:
    """请求级原子性：任何字段非法都在 mutation 之前退出（含 model_id 前置查表）。

    mutation 是有序的（template → model → …）：未知 model_id 若在 `_apply_model`
    处才被发现，`{agent: X, model_id: <未知>}` 会**先切模板、落盘三元组、再报错**
    ——错误响应 + 已生效的变更同时出现。前置查表让这条组合整体拒绝。
    """

    @pytest.mark.asyncio
    async def test_unknown_model_id_blocks_template_switch(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        from wing.config import AgentConfig

        root = tmp_path / "sessions"
        cfg = Config(
            providers=[
                ProviderConfig(
                    name="p",
                    base_url="http://x",
                    api_key="k",
                    models=[ModelSpec(id="a-id", name="a-name")],
                )
            ],
            agents=[
                AgentConfig(name="default", model="a-id"),
                AgentConfig(name="coder", model="a-id"),
            ],
        )
        _use_config(monkeypatch, cfg)
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id
        agent_before = session.agent

        coder = sm.template_manager.get("coder")
        assert coder is not None
        with pytest.raises(ValueError, match="unknown model id 'nope'"):
            await session.update_state(template=coder, model_id="nope")

        # 模板未切换、agent 未重建、三元组未变、磁盘零记录
        assert session.agent is agent_before
        assert session.template_name == "default"
        assert session.model_id == "a-id"
        assert session.agent.model == "a-name"
        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is None or (
            meta.template_name is None
            and meta.model_id is None
            and meta.model_name is None
        )

    @pytest.mark.asyncio
    async def test_valid_model_id_and_template_apply_together(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """对照：合法组合下两者都生效（观察点有效，不是把功能一起锁死）。"""
        from wing.config import AgentConfig

        root = tmp_path / "sessions"
        cfg = Config(
            providers=[
                ProviderConfig(
                    name="p",
                    base_url="http://x",
                    api_key="k",
                    models=[
                        ModelSpec(id="a-id", name="a-name"),
                        ModelSpec(id="b-id", name="b-name"),
                    ],
                )
            ],
            agents=[
                AgentConfig(name="default", model="a-id"),
                AgentConfig(name="coder", model="a-id"),
            ],
        )
        _use_config(monkeypatch, cfg)
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        sid = session.session_id

        coder = sm.template_manager.get("coder")
        assert coder is not None
        await session.update_state(template=coder, model_id="b-id")

        assert session.template_name == "coder"
        assert session.model_id == "b-id"
        assert session.agent.model == "b-name"
        meta = FileSessionStore(root).load_metadata(sid)
        assert meta is not None
        assert (meta.template_name, meta.model_id) == ("coder", "b-id")
