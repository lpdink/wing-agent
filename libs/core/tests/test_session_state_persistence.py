"""会话状态持久化回归测试——resume / fork 后请求前缀与重启前一致。

覆盖（与 test_model_persistence.py 同构，针对本轮新增的字段）：

- 显式动作落盘：update_state（thinking / effort / yolo / tools）与创建
  override（system_prompt / append_system_prompt / tools / max_turns /
  effort / yolo）；
- 跨进程重启后的 resume 还原（同一存储根目录新建 SessionManager）；
- before_session_start hook 注入的 append_system_prompt 随创建落盘
  （fork/resume 不再丢失 → 系统提示词逐字节不变 → KV cache 前缀命中）；
- fork 快照：子会话 metadata 记录 fork 时刻**有效状态**（含存量会话的
  live append 值），子会话重启后不偏离；
- 模板切换以新模板为准：清掉显式覆盖记录，append（会话级内容）保留；
- 降级容错：记录的工具 ref 不可解析时跳过/回退模板工具集，不阻断 resume；
- 兼容：未显式动作不写新字段（无记录 = 跟随模板/配置）。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wing.agent_template import AgentTemplate
from wing.gateway.protocol import AgentOverride
from wing.hook_registry import hooks
from wing.schema import Message
from wing.session_manager import SessionManager
from wing.store import FileSessionStore

# ============================================================
# fixtures（与 test_model_persistence.py 同构）
# ============================================================


@pytest.fixture
def root(tmp_path: Path) -> Path:
    """文件后端存储根目录（Gateway 的 session 目录）。"""
    return tmp_path / "sessions"


@pytest.fixture
def sm(root: Path) -> SessionManager:
    return SessionManager({"file": FileSessionStore(root)})


def _restart(root: Path) -> SessionManager:
    """在同一存储根目录上新建 SessionManager——模拟 Gateway 重启。"""
    return SessionManager({"file": FileSessionStore(root)})


def _seed(session, *contents: str) -> None:
    """向 session 的上下文写入消息（不经 LLM）。"""
    for i, content in enumerate(contents):
        role = "user" if i % 2 == 0 else "assistant"
        session.context_manager.add_message(Message(role=role, content=content))


def _raw_metadata(root: Path, sid: str) -> dict:
    return json.loads((root / sid / "metadata.json").read_text(encoding="utf-8"))


# ============================================================
# update_state（运行时切换）落盘
# ============================================================


class TestUpdateStatePersists:
    """session/update 的每个动态字段落盘并在重启后还原。"""

    @pytest.mark.asyncio
    async def test_options_and_tools_restored_after_restart(self, sm, root):
        session = sm.create_session()
        sid = session.session_id

        await session.update_state(
            thinking=False,
            reasoning_effort="low",
            yolo=True,
            tools=["Read", "Glob"],
        )

        meta = _raw_metadata(root, sid)
        assert meta["thinking"] is False
        assert meta["reasoning_effort"] == "low"
        assert meta["yolo"] is True
        assert sorted(meta["tools"]) == ["Glob", "Read"]

        restored = _restart(root).resume_session(sid)
        assert restored.agent.model_provider.thinking is False
        assert restored.agent.model_provider.reasoning_effort == "low"
        assert restored.agent.yolo is True
        assert [t.name for t in restored.agent.tools] == ["Glob", "Read"]

    @pytest.mark.asyncio
    async def test_model_switch_keeps_recorded_options(self, sm, root):
        """跨 provider 切模型后，记录在案的 thinking/effort 不悄悄重置。"""
        session = sm.create_session()
        await session.update_state(thinking=False, reasoning_effort="high")

        await session.update_state(model="qwen-max", provider_name="alt")

        assert session.agent.model_provider.name == "alt"
        assert session.agent.model_provider.thinking is False
        assert session.agent.model_provider.reasoning_effort == "high"


# ============================================================
# 创建 override 落盘
# ============================================================


class TestCreateOverridePersists:
    """创建时的 AgentOverride 是显式动作，字段全部落盘、resume 还原。"""

    @pytest.mark.asyncio
    async def test_override_state_restored_after_restart(self, sm, root):
        session = sm.create_session(
            agent_override=AgentOverride(
                system_prompt="Custom base.",
                append_system_prompt="Appended env info.",
                tools=["Read"],
                max_turns=7,
                effort="high",
                yolo=True,
            )
        )
        sid = session.session_id
        source_prompt = session.context_manager.system_prompt.content

        restored = _restart(root).resume_session(sid)
        cm = restored.context_manager
        assert cm.setin_system_prompt == "Custom base."
        assert cm.append_system_prompt == "Appended env info."
        assert [t.name for t in restored.agent.tools] == ["Read"]
        assert restored.agent.max_turns == 7
        assert restored.agent.model_provider.reasoning_effort == "high"
        assert restored.agent.yolo is True
        # 系统提示词逐字节一致（KV cache 前缀稳定的前提）
        assert cm.system_prompt.content == source_prompt

    @pytest.mark.asyncio
    async def test_append_merges_with_hook_injection(self, sm, root):
        """override 追加与 hook 注入按顺序合并进同一字段并落盘。"""

        def hook_inject(session, **ctx):  # noqa: ANN001 - hook 契约签名
            session.context_manager.append_to_system_prompt("<os>probe</os>")

        hooks.on("before_session_start")(hook_inject)
        try:
            session = sm.create_session(
                agent_override=AgentOverride(append_system_prompt="From override.")
            )
            sid = session.session_id
            assert (
                session.context_manager.append_system_prompt
                == "From override.\n<os>probe</os>"
            )
            raw = _raw_metadata(root, sid)
            assert raw["append_system_prompt"] == "From override.\n<os>probe</os>"

            restored = _restart(root).resume_session(sid)
            assert (
                restored.context_manager.append_system_prompt
                == "From override.\n<os>probe</os>"
            )
        finally:
            # 注册表没有公开的逐 handler 注销口，测试直接摘除（清空全局会波及其他用例）
            hooks._handlers.get("before_session_start", []).remove(hook_inject)


class TestHookInjectionIsCreationOnly:
    """before_session_start 只在建立新会话时触发；恢复既有会话不重复注入。"""

    @pytest.mark.asyncio
    async def test_recovering_existing_session_does_not_reappend(self, sm, root):
        calls: list[str] = []

        def hook_inject(session, **ctx):  # noqa: ANN001 - hook 契约签名
            calls.append(session.session_id)
            session.context_manager.append_to_system_prompt("<os>probe</os>")

        hooks.on("before_session_start")(hook_inject)
        try:
            session = sm.create_session(
                agent_override=AgentOverride(append_system_prompt="From override.")
            )
            sid = session.session_id
            assert (
                session.context_manager.append_system_prompt
                == "From override.\n<os>probe</os>"
            )
            assert calls == [sid]

            # 同一 session_id 的磁盘恢复：走 resume 语义，hook 不再触发
            recovered = sm.create_session(session_id=sid)
            assert (
                recovered.context_manager.append_system_prompt
                == "From override.\n<os>probe</os>"
            )
            assert calls == [sid], "恢复既有会话不得重跑 before_session_start"

            # 记录也没被叠加（重启后仍逐字节一致）
            restored = _restart(root).resume_session(sid)
            assert (
                restored.context_manager.append_system_prompt
                == "From override.\n<os>probe</os>"
            )
        finally:
            hooks._handlers.get("before_session_start", []).remove(hook_inject)


class TestTemplateSwitch:
    """模板切换以新模板为准：清覆盖记录、保留 append。"""

    @pytest.mark.asyncio
    async def test_switch_clears_overrides_keeps_append(self, sm, root):
        session = sm.create_session(
            agent_override=AgentOverride(
                system_prompt="Custom base.",
                append_system_prompt="Session env info.",
                tools=["Read"],
                max_turns=5,
                effort="low",
                yolo=True,
            )
        )
        sid = session.session_id

        template = AgentTemplate(name="coder", model="claude-x", provider_name="alt")
        await session.switch_template(template)

        # live：新模板的提示词 / 工具 / 开关；append 是会话级内容，保留
        cm = session.context_manager
        assert cm.setin_system_prompt == template.system_prompt
        assert cm.append_system_prompt == "Session env info."

        # 重启后与 live 一致（清掉的记录不会把旧覆盖贴回来）
        restored = _restart(root).resume_session(sid)
        rcm = restored.context_manager
        assert rcm.setin_system_prompt == template.system_prompt
        assert rcm.append_system_prompt == "Session env info."
        assert restored.agent.max_turns is None


# ============================================================
# fork 快照
# ============================================================


class TestForkSnapshot:
    """fork 记录源会话在 fork 时刻的有效状态（快照语义）。"""

    @pytest.mark.asyncio
    async def test_fork_carries_effective_state(self, sm, root):
        source = sm.create_session(
            agent_override=AgentOverride(
                system_prompt="Source base.",
                append_system_prompt="Source env.",
                tools=["Read", "Glob"],
                max_turns=11,
                effort="high",
                yolo=True,
            )
        )
        await source.update_state(thinking=False)
        _seed(source, "hello", "question")
        target = source.context_manager.get_context_window()[-1].uuid

        child, _ = sm.fork_session(source.session_id, target)
        assert child is not None
        child_sid = child.session_id

        # 子会话 live 与源会话一致
        assert (
            child.context_manager.system_prompt.content
            == source.context_manager.system_prompt.content
        )
        assert [t.name for t in child.agent.tools] == ["Glob", "Read"]

        # 重启后仍一致（快照写入 metadata）
        restored = _restart(root).resume_session(child_sid)
        assert (
            restored.context_manager.system_prompt.content
            == source.context_manager.system_prompt.content
        )
        assert [t.name for t in restored.agent.tools] == ["Glob", "Read"]
        assert restored.agent.max_turns == 11
        assert restored.agent.model_provider.thinking is False
        assert restored.agent.model_provider.reasoning_effort == "high"
        assert restored.agent.yolo is True

    @pytest.mark.asyncio
    async def test_fork_does_not_materialize_derived_provider_defaults(self, sm, root):
        """未动过开关的会话 fork：thinking / effort 不留记录（交给 provider 配置）。

        派生默认（如 anthropic 未配置 thinking 时的 disabled）被固化进记录后，
        会让子会话请求体带上源会话没有的显式配置——前缀身份被破坏。
        """
        source = sm.create_session()
        _seed(source, "hello", "question")
        target = source.context_manager.get_context_window()[-1].uuid

        child, _ = sm.fork_session(source.session_id, target)
        assert child is not None

        raw = _raw_metadata(root, child.session_id)
        assert "thinking" not in raw, raw
        assert "reasoning_effort" not in raw, raw
        # provider 级状态随配置派生：子会话与源会话一致
        assert (
            child.agent.model_provider.thinking == source.agent.model_provider.thinking
        )

    @pytest.mark.asyncio
    async def test_fork_copies_recorded_provider_options(self, sm, root):
        """显式动作切换过的开关：记录原样进子会话（fork 快照的一部分）。"""
        source = sm.create_session()
        await source.update_state(thinking=False, reasoning_effort="low")
        _seed(source, "hello", "question")
        target = source.context_manager.get_context_window()[-1].uuid

        child, _ = sm.fork_session(source.session_id, target)
        assert child is not None

        raw = _raw_metadata(root, child.session_id)
        assert raw["thinking"] is False
        assert raw["reasoning_effort"] == "low"

        restored = _restart(root).resume_session(child.session_id)
        assert restored.agent.model_provider.thinking is False
        assert restored.agent.model_provider.reasoning_effort == "low"

    @pytest.mark.asyncio
    async def test_fork_copies_live_append_without_record(self, sm, root):
        """存量会话（append 未落盘、只在内存）fork 时按 live 值拷贝。"""
        source = sm.create_session()
        # 模拟"字段引入前创建的会话"：hook 注入只存在于内存
        source.context_manager.append_to_system_prompt(
            "<current_directory>/x</current_directory>"
        )
        _seed(source, "hello", "question")
        target = source.context_manager.get_context_window()[-1].uuid

        child, _ = sm.fork_session(source.session_id, target)
        assert child is not None

        raw = _raw_metadata(root, child.session_id)
        assert (
            raw["append_system_prompt"] == "<current_directory>/x</current_directory>"
        )

        restored = _restart(root).resume_session(child.session_id)
        assert (
            restored.context_manager.append_system_prompt
            == "<current_directory>/x</current_directory>"
        )


# ============================================================
# 降级容错与兼容
# ============================================================


class TestToolsDegradation:
    """记录的工具 ref 不可解析：跳过失效项，不阻断 resume。"""

    @pytest.mark.asyncio
    async def test_partial_unresolvable_keeps_resolvable_subset(self, sm, root):
        session = sm.create_session(agent_override=AgentOverride(tools=["Read"]))
        sid = session.session_id
        session._metadata.tools = ["Read", "ghost.DeadTool"]
        session._save_metadata()

        restored = _restart(root).resume_session(sid)
        assert [t.name for t in restored.agent.tools] == ["Read"]

    @pytest.mark.asyncio
    async def test_all_unresolvable_keeps_template_tools(self, sm, root):
        session = sm.create_session()
        sid = session.session_id
        session._metadata.tools = ["ghost.DeadTool"]
        session._save_metadata()

        restored = _restart(root).resume_session(sid)
        assert sorted(t.name for t in restored.agent.tools) == sorted(
            t.name for t in sm.template_manager.default.resolved_tools
        )


class TestLiveSkewAndEmptyRecords:
    """不落记录的 live 变更（yolo）与显式空记录（tools=[]）的还原语义。"""

    @pytest.mark.asyncio
    async def test_unrecorded_live_yolo_survives_model_switch(self, sm):
        """跨 provider 切模型只重贴 provider 级开关，不覆盖 live 的 agent 级状态。

        Bash 工具的 "always allow (yolo)" 只改内存、不落记录——切模型不得
        静默撤销用户刚在确认框里做出的选择。
        """
        session = sm.create_session()
        await session.update_state(yolo=False)
        session.agent.set_yolo(True)

        await session.update_state(model="qwen-max", provider_name="alt")

        assert session.agent.model_provider.name == "alt"
        assert session.agent.yolo is True

    @pytest.mark.asyncio
    async def test_explicit_empty_tools_restored(self, sm, root):
        """`tools: []` 是显式动作（清空工具集），不能与「无记录」混淆。"""
        session = sm.create_session(agent_override=AgentOverride(tools=[]))
        sid = session.session_id
        assert session.agent.tools == []
        assert _raw_metadata(root, sid)["tools"] == []

        restored = _restart(root).resume_session(sid)
        assert restored.agent.tools == []


class TestCompatibility:
    """未显式动作不写新字段（缺失记录 = 跟随模板/配置）。"""

    @pytest.mark.asyncio
    async def test_untouched_session_writes_no_records(self, sm, root):
        session = sm.create_session()
        sid = session.session_id
        session.touch_last_interaction()  # 触发一次 metadata 落盘

        raw = _raw_metadata(root, sid)
        for field in (
            "system_prompt",
            "append_system_prompt",
            "tools",
            "thinking",
            "reasoning_effort",
            "yolo",
            "max_turns",
        ):
            assert field not in raw, field
