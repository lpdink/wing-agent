"""会话列表条目的**模型投影**（``GET /api/session/list`` 的模型四件套）。

列表是跨会话的唯一视图：``/api/session/info`` 只回答「这一个会话在跑什么」，
逐会话去问就是 N+1。因此条目自己带生效模型——在内存的会话取 live agent，
未加载的按 resume 链解析盘上记录（记录命中当前映射 → 快照兜底 → 模板默认），
与 ``Session._restore_persisted_model``（真的把记录装回 agent 的那条写路径）
给出**同一个答案**（`TestListMatchesResume` 直接对账两者）。

覆盖：

- active / inactive 两种形态都携带四件套；
- 记录命中当前映射（name / provider 跟随配置演化）、旧记录（无 id）经反查补 id、
  id 与调用名都不在声明里时只降级 id 位（调用名照旧是恢复线索）；
- 无记录 / 记录不完整 / 快照 provider 不可用 / 模板名已不存在 → 模板默认
  （metadata 是模板的唯一来源）；
- 展示名未声明 / 空串 = None（网关不发空串，展示层回落调用名）；
- 列表顺序不因新增字段而变（status / 时间 / id 定序的口径不动）。
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

import pytest

from wing.config import AgentConfig, Config, ModelSpec, ProviderConfig
from wing.event import SessionInfo
from wing.schema import Message
from wing.session import SessionManager
from wing.store import FileSessionStore


def _sid(label: str) -> str:
    """测试用 session id（存储把 id 当路径组件，必须契合既定格式）。"""
    digest = hashlib.md5(label.encode()).hexdigest()[:8]
    return f"20250101-000000-{digest}"


def _use_config(
    monkeypatch: pytest.MonkeyPatch,
    *,
    models: list[str | ModelSpec],
    agents: list[tuple[str, str]],
    provider_name: str = "p",
) -> Config:
    """替换配置单例（与 conftest 的 autouse fixture 同一入口）。

    ``agents`` 是 ``(模板名, model id)`` 列表：首个即默认模板——「模板默认」这一
    级的答案由此而来。
    """
    import wing.config.loader as loader

    cfg = Config(
        providers=[
            ProviderConfig(
                name=provider_name,
                base_url="http://x",
                api_key="k",
                models=models,
            )
        ],
        agents=[AgentConfig(name=name, model=model) for name, model in agents],
    )
    monkeypatch.setattr(loader, "_config", cfg)
    return cfg


def _write_session(
    root: Path,
    session_id: str,
    *,
    name: str = "a session",
    **record: Any,
) -> None:
    """在盘上构造一个会话目录（metadata.json + 一条 user 消息的 history.jsonl）。

    ``record`` 是模型记录的键值（``model_id`` / ``model_name`` / ``provider_name``
    / ``template_name`` …）；值为 None 时**不写该键**——"记录里没有"与"记录是
    null"在恢复链里同价，但不写字面 null 更贴近真实产物。
    """
    session_dir = root / session_id
    session_dir.mkdir(parents=True, exist_ok=True)
    (session_dir / "history.jsonl").write_text(
        json.dumps({"role": "user", "content": name, "uuid": f"{session_id}-u1"})
        + "\n",
        encoding="utf-8",
    )
    metadata = {"session_name": name}
    metadata.update({k: v for k, v in record.items() if v is not None})
    (session_dir / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")


def _entry(sm: SessionManager, session_id: str) -> SessionInfo:
    entries = {s.id: s for s in sm.list_sessions()}
    assert session_id in entries, list(entries)
    return entries[session_id]


def _model_quartet(
    info: SessionInfo,
) -> tuple[str | None, str | None, str | None, str | None]:
    """条目的模型四件套（断言用的统一取法）。"""
    return (
        info.model_id,
        info.model_name,
        info.provider_name,
        info.model_display_name,
    )


#: 场景 1：id ≠ 调用名，且声明了展示名（三个字段各自可分辨）。
FLASH = ModelSpec(
    id="ds-flash", name="deepseek-flash-2026", display_name="DeepSeek Flash"
)


class TestRecordProjection:
    """未加载（inactive）会话：按 resume 链解析盘上记录。"""

    def test_recorded_id_projects_id_name_provider_and_display_name(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(monkeypatch, models=[FLASH], agents=[("default", "ds-flash")])
        root = tmp_path / "sessions"
        sid = _sid("recorded")
        _write_session(
            root,
            sid,
            model_id="ds-flash",
            model_name="deepseek-flash-2026",
            provider_name="p",
            template_name="default",
        )

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        assert entry.status == "inactive"
        assert _model_quartet(entry) == (
            "ds-flash",
            "deepseek-flash-2026",
            "p",
            "DeepSeek Flash",
        )

    def test_recorded_id_follows_the_current_mapping(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """id 命中 → **当前映射**：声明改名 / 换展示名后，列表跟着配置走。

        与 resume 同口径（id 是引用词，name 只是发给上游的值）：盘上的快照是
        写入时刻的事实，不是列表要复读的值。
        """
        root = tmp_path / "sessions"
        sid = _sid("renamed")
        _write_session(
            root,
            sid,
            model_id="ds-flash",
            model_name="deepseek-flash-2026",
            provider_name="p",
            template_name="default",
        )
        _use_config(
            monkeypatch,
            models=[ModelSpec(id="ds-flash", name="renamed-upstream")],
            agents=[("default", "ds-flash")],
        )

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        # 未声明展示名的模型 → None（展示层回落调用名）。
        assert _model_quartet(entry) == ("ds-flash", "renamed-upstream", "p", None)

    def test_legacy_record_backfills_the_id_from_the_snapshot(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """旧记录（无 ``model_id``）：快照兜底 + ``identify`` 反查补 id。"""
        _use_config(monkeypatch, models=[FLASH], agents=[("default", "ds-flash")])
        root = tmp_path / "sessions"
        sid = _sid("legacy")
        _write_session(
            root,
            sid,
            model_id=None,
            model_name="deepseek-flash-2026",
            provider_name="p",
        )

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        assert _model_quartet(entry) == (
            "ds-flash",
            "deepseek-flash-2026",
            "p",
            "DeepSeek Flash",
        )

    def test_undeclared_name_degrades_the_id_slot_only(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """id 被删且调用名也不在声明里：引用词降级为 None，快照照旧下发。

        记录里的旧 id 已不是引用词（配置里没有它），不编一个假的；快照
        （调用名 + provider）才是"接着跑会打到哪"的答案，不能被当成"没有模型"
        ——resume 同样继续用快照跑。
        """
        _use_config(monkeypatch, models=[FLASH], agents=[("default", "ds-flash")])
        root = tmp_path / "sessions"
        sid = _sid("ghost")
        _write_session(
            root,
            sid,
            model_id="deleted-id",
            model_name="ghost-upstream",
            provider_name="p",
        )

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        assert _model_quartet(entry) == (None, "ghost-upstream", "p", None)


class TestTemplateDefaultFallback:
    """没有任何可用记录：模板默认（metadata 是模板的唯一来源）。"""

    def test_session_without_a_model_record_uses_its_own_template(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            models=[FLASH, ModelSpec(id="other-id", name="other-upstream")],
            agents=[("default", "ds-flash"), ("other", "other-id")],
        )
        root = tmp_path / "sessions"
        sid = _sid("no-record")
        _write_session(root, sid, template_name="other")

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        assert _model_quartet(entry) == ("other-id", "other-upstream", "p", None)

    def test_missing_template_name_falls_back_to_the_default_template(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """模板名已不存在（config 演化 / 手改）：与 resume 同一出口 → 默认模板。"""
        _use_config(
            monkeypatch,
            models=[FLASH, ModelSpec(id="other-id", name="other-upstream")],
            agents=[("default", "ds-flash"), ("other", "other-id")],
        )
        root = tmp_path / "sessions"
        sid = _sid("gone-template")
        _write_session(root, sid, template_name="gone")

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        assert _model_quartet(entry) == (
            "ds-flash",
            "deepseek-flash-2026",
            "p",
            "DeepSeek Flash",
        )

    def test_incomplete_record_falls_back_to_the_template(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """记录不完整（只有半条快照）：不当半截证据用，落模板默认。"""
        _use_config(monkeypatch, models=[FLASH], agents=[("default", "ds-flash")])
        root = tmp_path / "sessions"
        sid = _sid("half-record")
        _write_session(root, sid, model_name="only-a-name")

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        assert _model_quartet(entry) == (
            "ds-flash",
            "deepseek-flash-2026",
            "p",
            "DeepSeek Flash",
        )

    def test_unresolvable_provider_falls_back_to_the_template(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """快照 provider 不可解析（配置里已删）：回落模板默认（记录保留在盘上）。"""
        _use_config(monkeypatch, models=[FLASH], agents=[("default", "ds-flash")])
        root = tmp_path / "sessions"
        sid = _sid("gone-provider")
        _write_session(
            root,
            sid,
            model_name="m",
            provider_name="gone",
        )

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        assert _model_quartet(entry) == (
            "ds-flash",
            "deepseek-flash-2026",
            "p",
            "DeepSeek Flash",
        )

    def test_blank_display_name_is_reported_as_none(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """空串 / 纯空白的展示名 = 未声明（网关只发 null，不发空串）。"""
        _use_config(
            monkeypatch,
            models=[
                ModelSpec(id="blank-id", name="blank-upstream", display_name="   ")
            ],
            agents=[("default", "blank-id")],
        )
        root = tmp_path / "sessions"
        sid = _sid("blank-display")
        _write_session(root, sid, model_id="blank-id")

        entry = _entry(SessionManager({"file": FileSessionStore(root)}), sid)

        assert entry.model_display_name is None
        assert entry.model_name == "blank-upstream"


class TestActiveProjection:
    """已加载（active）会话：四件套取自 live agent，不走盘上记录。"""

    @pytest.mark.asyncio
    async def test_live_agent_wins_over_the_disk_record(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            models=[FLASH, ModelSpec(id="other-id", name="other-upstream")],
            agents=[("default", "other-id")],
        )
        root = tmp_path / "sessions"
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        # 盘上记录指向模板默认模型；live agent 切到另一个模型 → 列表必须听 live 的。
        assert session.model_id == "other-id"
        await session.update_state(model_id="ds-flash")
        # 列表的候选判据是"有 history 或有标"——给一条消息让它在列表里成立。
        session.context_manager.add_message(Message(role="user", content="hello"))

        entry = _entry(sm, session.session_id)
        binding = session.model_binding()

        assert entry.status != "inactive"
        assert _model_quartet(entry) == (
            "ds-flash",
            "deepseek-flash-2026",
            "p",
            "DeepSeek Flash",
        )
        # 与 live 会话自己的投影同源（同一份事实，两个出口）。
        assert _model_quartet(entry) == (
            binding.model_id,
            binding.model_name,
            binding.provider_name,
            binding.model_display_name,
        )
        assert session.to_agent_info().model_display_name == "DeepSeek Flash"


class TestListSurvivesBrokenConfig:
    """列表是跨会话视图：单个会话的配置问题不许拖垮整个端点。

    历史上列表对 live 会话只读 `status`，不碰 agent 的模型解析；现在它要读模型
    四件套——展示名经 provider 解析，而 provider 可能已从配置里删掉（池里恰好也
    没有旧实例）。那一路必须降级（四件套里只有展示名变 None），而不是抛错。
    """

    @pytest.mark.asyncio
    async def test_provider_removed_from_config_degrades_the_display_name(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        # 只有一个 provider 的配置：下一步它会整个从配置里消失。
        _use_config(
            monkeypatch,
            models=[FLASH],
            agents=[("default", "ds-flash")],
            provider_name="vanishing-provider",
        )
        root = tmp_path / "sessions"
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        session.context_manager.add_message(Message(role="user", content="hello"))

        # 换配置：provider 没了，且共享池里也没有它的实例（会话从未发过请求）。
        _use_config(
            monkeypatch,
            models=[FLASH],
            agents=[("default", "ds-flash")],
            provider_name="other-provider",
        )

        entry = _entry(sm, session.session_id)

        # 身份与运行期事实照旧（它们不依赖 provider 解析），只有展示名降级。
        assert entry.model_id == "ds-flash"
        assert entry.model_name == "deepseek-flash-2026"
        assert entry.provider_name == "vanishing-provider"
        assert entry.model_display_name is None


class TestListMatchesResume:
    """列表投影 == resume 真跑的模型（同一个答案，两条路径）。

    这是本投影存在的全部理由：用户在列表里看到 A、resume 后却跑在 B，是同一件
    事的两种说法。恢复链两侧（``Session._restore_persisted_model`` 与
    ``resolve_model_binding``）任何一侧单独改动都会在这里露出来。
    """

    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        "record",
        [
            pytest.param(
                {
                    "model_id": "ds-flash",
                    "model_name": "deepseek-flash-2026",
                    "provider_name": "p",
                },
                id="id-hit",
            ),
            pytest.param(
                {"model_name": "deepseek-flash-2026", "provider_name": "p"},
                id="legacy-snapshot",
            ),
            pytest.param(
                {"model_id": "deleted-id", "model_name": "ghost", "provider_name": "p"},
                id="stale-id",
            ),
            pytest.param({"template_name": "other"}, id="no-record"),
            pytest.param(
                {"model_name": "ghost", "provider_name": "gone"},
                id="unresolvable-provider",
            ),
            # 损坏记录（空串快照）：两侧的"记录存在吗"必须同一判据（`is not
            # None`），否则列表说模板默认、resume 却把空名字装回 agent。
            pytest.param({"model_name": "", "provider_name": "p"}, id="empty-name"),
            pytest.param(
                {"model_name": "ghost", "provider_name": ""}, id="empty-provider"
            ),
        ],
    )
    async def test_list_quartet_equals_the_resumed_binding(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, record: dict
    ):
        _use_config(
            monkeypatch,
            models=[FLASH, ModelSpec(id="other-id", name="other-upstream")],
            agents=[("default", "ds-flash"), ("other", "other-id")],
        )
        root = tmp_path / "sessions"
        sid = _sid(f"resume-eq-{sorted(record)}")
        _write_session(root, sid, **record)

        sm = SessionManager({"file": FileSessionStore(root)})
        entry = _entry(sm, sid)

        resumed = sm.resume_session(sid)
        binding = resumed.model_binding()

        assert _model_quartet(entry) == (
            binding.model_id,
            binding.model_name,
            binding.provider_name,
            binding.model_display_name,
        )


class TestOrderIsUntouched:
    """模型字段不参与排序：活跃优先 + 时间降序 + id 定序的口径原地不动。"""

    def test_model_fields_do_not_reorder_the_list(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            models=[FLASH, ModelSpec(id="other-id", name="other-upstream")],
            agents=[("default", "ds-flash"), ("other", "other-id")],
        )
        root = tmp_path / "sessions"
        recent = _sid("recent")
        older = _sid("older")
        _write_session(
            root,
            older,
            name="older",
            model_id="other-id",
            model_name="other-upstream",
            provider_name="p",
            last_interaction="2025-01-01T00:00:00",
        )
        _write_session(
            root,
            recent,
            name="recent",
            model_id="ghost-id",
            model_name="ghost",
            provider_name="p",
            last_interaction="2025-06-01T00:00:00",
        )

        ids = [
            s.id
            for s in SessionManager({"file": FileSessionStore(root)}).list_sessions()
        ]

        assert ids == [recent, older]
