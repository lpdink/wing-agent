# wing/system.py
"""wing/system.py — 系统级热重载流程（11 归位：自 ``WingRuntime.reload_system`` 抽出）。

``reload_system(sm)`` 是 ``/api/system/reload`` 的实现体：config → hooks → prompt
commands → provider → skills & rules，逐项独立 try/except；config 失败立即中止
（后续项不再尝试），其余项失败继续——**步骤顺序与逐项 detail 是对外契约**
（probe ``test_system_reload`` 抓名字序与逐项 ok）。

纯移动：``ReloadResult`` / ``ReloadResultItem`` 与流程体逐字来自 runtime，唯一
机械差异是 ``self.sm`` → 参数 ``sm``（调用侧 ``WingRuntime.reload_system()`` 只
保留一行委托）。函数体内的 import 保持在函数体内（懒加载）——``wing.config.load_config``
等名字在调用时刻解析，保持可 monkeypatch（``tests/test_runtime_session_ops.py``）。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from wing.session import SessionManager

# ============================================================
# 公共数据类
# ============================================================


@dataclass
class ReloadResultItem:
    """reload_system 中单项重载的结果。"""

    name: str
    ok: bool
    detail: str | None = None


@dataclass
class ReloadResult:
    """reload_system 的完整结果。"""

    ok: bool
    items: list[ReloadResultItem] = field(default_factory=list)


# ============================================================
# reload_system
# ============================================================


async def reload_system(sm: SessionManager) -> ReloadResult:
    """热重载全局配置、hooks、prompt commands、provider、skills & rules。

    config 加载失败时立即中止。其余项失败时继续。
    """
    from wing.commands import magic_registry, register_prompt_commands
    from wing.config import load_config
    from wing.hooks import hooks, load_hooks

    items: list[ReloadResultItem] = []

    # 1. Reload config
    try:
        config = load_config(reload=True)
        items.append(ReloadResultItem(name="config.yaml", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="config.yaml", ok=False, detail=str(e)))
        return ReloadResult(ok=False, items=items)

    # 2. Reload hooks
    try:
        hooks.clear()
        load_hooks(config.hooks)
        items.append(ReloadResultItem(name="hooks", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="hooks", ok=False, detail=str(e)))

    # 3. Reload prompt commands
    try:
        magic_registry.remove_by_source("prompt")
        register_prompt_commands(config.commands.paths)
        items.append(ReloadResultItem(name="prompt commands", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="prompt commands", ok=False, detail=str(e)))

    # 4. Rebuild provider clients — 所有 session（驱逐重建：按新配置重建
    #    活跃 provider 后关闭旧 client；配置变更随重建自然生效）。
    #    模型列表 registry 一并重置（下次查询按新配置重建）。
    #    单 session 失败不阻断其余 session（否则一个坏 session 会让其他
    #    session 悄悄留着旧凭据——正是驱逐重建要修的 bug）。
    try:
        from wing.provider.registry import reset_registry

        await reset_registry()
        rebuilt = 0
        failures: list[str] = []
        for session in sm.iter_sessions():
            try:
                # 快照遍历期间可能发生逐出/拆解：已不在内存的会话跳过，
                # 否则会给已关闭 provider 的 agent 重建 client 且无人回收。
                if sm.get_session(session.session_id) is None:
                    continue
                await session.agent.rebuild_providers()
                # provider 实例换了：记录在案的 provider 级开关（thinking /
                # reasoning_effort）重贴，否则 reload 后 live 悄悄退回配置
                # 默认、请求前缀随之漂移（Session 持有记录，见其 docstring）。
                session.reapply_provider_options()
                rebuilt += 1
            except Exception as e:
                failures.append(f"{session.session_id}: {e}")
        detail = f"rebuilt {rebuilt} session(s)"
        if failures:
            detail += "; failed: " + ", ".join(failures)
        items.append(ReloadResultItem(name="provider", ok=not failures, detail=detail))
    except Exception as e:
        items.append(ReloadResultItem(name="provider", ok=False, detail=str(e)))

    # 5. Reload skills & rules for all active sessions
    try:
        for session in sm.iter_sessions():
            session.agent.context_manager.reload_skills_and_rules()
        items.append(ReloadResultItem(name="skills & rules", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="skills & rules", ok=False, detail=str(e)))

    all_ok = all(item.ok for item in items)
    return ReloadResult(ok=all_ok, items=items)
