"""Session.update_state() 单元测试。

测试 Session 自身的状态变更逻辑：执行顺序、各分支是否正确调用。
使用真实的 Session 实例（通过 SessionManager 创建），而非 mock。
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest

from wing.session import SessionManager
from wing.store import FileSessionStore


@pytest.fixture
def sm():
    """创建 SessionManager（memory 后端，不落盘）。"""
    from wing.store import MemorySessionStore

    return SessionManager({"memory": MemorySessionStore()}, default_backend="memory")


class TestSessionUpdateState:
    """Session.update_state() 各字段独立测试。"""

    @pytest.mark.asyncio
    async def test_update_model(self, sm):
        """更新模型名称。"""
        session = sm.create_session()
        original_model = session.agent.model
        assert original_model != "gpt-4o-mini"

        await session.update_state(model="gpt-4o-mini")
        assert session.agent.model == "gpt-4o-mini"

    @pytest.mark.asyncio
    async def test_update_title(self, sm):
        """设置 session 标题。"""
        session = sm.create_session()
        assert session.session_name is None

        await session.update_state(title="My Test Session")
        assert session.session_name == "My Test Session"

    @pytest.mark.asyncio
    async def test_update_thinking(self, sm):
        """开关 thinking 模式。"""
        session = sm.create_session()
        original = session.agent.thinking

        await session.update_state(thinking=not original)
        assert session.agent.thinking == (not original)

    @pytest.mark.asyncio
    async def test_update_reasoning_effort(self, sm):
        """设置推理力度。"""
        session = sm.create_session()
        await session.update_state(reasoning_effort="high")
        assert session.agent.reasoning_effort == "high"

    @pytest.mark.asyncio
    async def test_update_yolo(self, sm):
        """开关 yolo 模式。"""
        session = sm.create_session()
        original = session.agent.yolo

        await session.update_state(yolo=not original)
        assert session.agent.yolo == (not original)

    @pytest.mark.asyncio
    async def test_none_params_skipped(self, sm):
        """所有参数为 None 时不执行任何操作。"""
        session = sm.create_session()
        original_model = session.agent.model
        original_name = session.session_name

        await session.update_state()

        assert session.agent.model == original_model
        assert session.session_name == original_name

    @pytest.mark.asyncio
    async def test_template_switch_called(self, sm):
        """template 不为 None 时调用 switch_template。"""
        session = sm.create_session()
        mock_template = MagicMock()
        session.switch_template = AsyncMock()

        await session.update_state(template=mock_template)
        session.switch_template.assert_awaited_once_with(mock_template)

    @pytest.mark.asyncio
    async def test_multiple_fields_updated(self, sm):
        """多字段同时更新，全部生效。"""
        session = sm.create_session()
        await session.update_state(
            model="gpt-4o-mini",
            title="Multi Update",
            thinking=True,
            reasoning_effort="medium",
            yolo=True,
        )

        assert session.agent.model == "gpt-4o-mini"
        assert session.session_name == "Multi Update"
        assert session.agent.thinking is True
        assert session.agent.reasoning_effort == "medium"
        assert session.agent.yolo is True


class TestModelDisplayName:
    """展示名穿透：配置声明 → agent 属性 → AgentInfo（前端展示的唯一素材）。"""

    @pytest.mark.asyncio
    async def test_agent_and_agent_info_follow_declaration(self, monkeypatch):
        import wing.config.loader

        from wing.config import AgentConfig, Config, ModelSpec, ProviderConfig
        from wing.store import MemorySessionStore

        cfg = Config(
            providers=[
                ProviderConfig(
                    name="p",
                    base_url="http://x",
                    api_key="k",
                    models=[
                        ModelSpec(name="fancy", display_name="Fancy Flash"),
                        "plain",
                    ],
                )
            ],
            agents=[AgentConfig(name="default", model="fancy", provider="p")],
        )
        # 单例替换（conftest 的 autouse fixture 也走这个口；直接 import 的函数
        # 读的是模块全局 _config，patch get_config 名字对它们无效）。
        monkeypatch.setattr(wing.config.loader, "_config", cfg)
        sm = SessionManager({"memory": MemorySessionStore()}, default_backend="memory")
        session = sm.create_session()
        assert session.agent.model_display_name == "Fancy Flash"
        assert session.to_agent_info().model_display_name == "Fancy Flash"

        # 切到无展示名声明的模型：回落 None（前端回落调用名）。
        await session.update_state(model="plain")
        assert session.agent.model == "plain"
        assert session.agent.model_display_name is None
        assert session.to_agent_info().model_display_name is None


class TestNonUtf8UpdateState:
    """update_state 的文本字段闸门：拒绝在 mutation 之前，活会话不被毒化。

    背景（终轮复审 S1）：`model` / `reasoning_effort` 先写内存态、再落 metadata；
    `_persist_model` 只在 `encode("utf-8")` 处抛 `UnicodeEncodeError`（`ValueError`
    子类，被路由当成"客户端的错"→ 400），但**内存态已经是非法值**——此后该会话
    `/info` 序列化就炸、所有写操作全失败（不落盘、逐出/重启自愈）。
    """

    @pytest.fixture
    def file_sm(self, tmp_path: Path) -> SessionManager:
        """file 后端：非法值必须真的走到"落盘失败"这一步才有意义。"""
        return SessionManager({"file": FileSessionStore(tmp_path / "sessions")})

    @pytest.mark.asyncio
    async def test_model_is_gated_before_mutation(self, file_sm: SessionManager):
        session = file_sm.create_session(session_id="U-1")
        before = session.agent.model

        with pytest.raises(ValueError) as failure:
            await session.update_state(model="\ud800")
        assert "model must be UTF-8 encodable" in str(failure.value)

        # 未被毒化：内存态还是老值，后续写操作照常，metadata 里没有模型记录
        assert session.agent.model == before
        await session.update_state(title="ok")  # 毒化时这里会抛 UnicodeEncodeError
        metadata = session.store.load_metadata("U-1")
        assert metadata is not None and metadata.session_name == "ok"
        assert metadata.model_name is None and metadata.provider_name is None

    @pytest.mark.asyncio
    async def test_reasoning_effort_is_gated_before_mutation(
        self, file_sm: SessionManager
    ):
        session = file_sm.create_session(session_id="U-2")
        before = session.agent.reasoning_effort

        with pytest.raises(ValueError) as failure:
            await session.update_state(reasoning_effort="\ud800")
        assert "reasoning_effort must be UTF-8 encodable" in str(failure.value)

        assert session.agent.reasoning_effort == before
        await session.update_state(title="ok")  # 后续写操作不受影响
        metadata = session.store.load_metadata("U-2")
        assert metadata is not None and metadata.session_name == "ok"
        assert metadata.reasoning_effort is None

    @pytest.mark.asyncio
    async def test_provider_is_gated_before_mutation(self, file_sm: SessionManager):
        """provider 单独给出时也先过闸门（不该靠"查找先抛"这种偶然顺序）。"""
        session = file_sm.create_session(session_id="U-3")
        with pytest.raises(ValueError) as failure:
            await session.update_state(provider_name="\ud800")
        assert "provider must be UTF-8 encodable" in str(failure.value)


class TestBestEffortStatePersistence:
    """纵深：`_record_state` / `_persist_model` 的 best-effort 口径也覆盖"编不了"。

    闸门之后仍有一条可达持久化的未编码路径：**hook 注入的文本**——
    `before_session_start` hook 调 `cm.append_to_system_prompt(...)`，create 末尾的
    `sync_append_system_prompt` 把它快照进 metadata（hook 是用户代码，读文件时用
    `surrogateescape` 就可能带出代理字符）。这时"写不进去"与 disk full 同价：
    报错只会让 create 在**已经注册会话之后**炸成 500；按 best-effort 记一条
    warning、live 状态继续生效，才是这两个方法既有的口径。
    """

    @pytest.mark.asyncio
    async def test_hook_injected_surrogate_does_not_raise(
        self, wing_warnings: list[str], tmp_path: Path
    ):
        sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        session = sm.create_session(session_id="H-1")

        session.context_manager.append_to_system_prompt("\ud800")  # hook 干的事
        session.sync_append_system_prompt()  # 不抛

        assert session.context_manager.append_system_prompt == "\ud800"  # live 生效
        assert any("state not persisted" in m for m in wing_warnings), wing_warnings
