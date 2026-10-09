# wing/system.py
"""wing/system.py — 系统级热重载流程（11 归位：自 ``WingRuntime.reload_system`` 抽出）。

``reload_system(sm)`` 是 ``/api/system/reload`` 的实现体：config → hooks → prompt
commands → provider → skills & rules → log level，逐项独立 try/except；config 失败立即中止
（后续项不再尝试），其余项失败继续——**步骤顺序与逐项 detail 是对外契约**
（probe ``test_system_reload`` 抓名字序与逐项 ok）。config 项内部还包含一步
``sm.reload_templates()``（模板管理器跟随新 config 重建，见 ``SessionManager.reload_templates``）。
``log level`` 是 03 追加的**末项**（既有五项的名字与顺序不许动）：``setup_logger`` 幂等重挂
handler，把声明的 ``log.level: hot`` 兑现到运行期。

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
    """热重载全局配置、hooks、prompt commands、provider、skills & rules、log level。

    config 加载失败时立即中止。其余项失败时继续。
    """
    from wing.commands import magic_registry, register_prompt_commands
    from wing.config import load_config
    from wing.hooks import hooks, load_hooks

    items: list[ReloadResultItem] = []

    try:
        config = load_config(reload=True)
        # 模板是 config 的派生状态（agents[].model 经 id 表解析）：与 config
        # 一起重建，避免「reload 后新会话用旧模板、resume 用新映射」分叉。
        # 不另立 ReloadResultItem——逐项名字序是对外契约，且 config 加载成功
        # 而模板重建失败在逻辑上不可达（加载期已强制 agents[].model ∈ id 空间）。
        sm.reload_templates()
        items.append(ReloadResultItem(name="config.yaml", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="config.yaml", ok=False, detail=str(e)))
        return ReloadResult(ok=False, items=items)

    try:
        hooks.clear()
        load_hooks(config.hooks)
        items.append(ReloadResultItem(name="hooks", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="hooks", ok=False, detail=str(e)))

    try:
        magic_registry.remove_by_source("prompt")
        register_prompt_commands(config.commands.paths)
        items.append(ReloadResultItem(name="prompt commands", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="prompt commands", ok=False, detail=str(e)))

    # provider 重建：共享池按新配置重建全部实例（先建后换——任一构建失败池保持
    # 原样）；旧实例退场（在途请求跑完后自动关闭，reload 对会话透明）。会话不再
    # 逐一重建：实例经池按 name 实时解析，新请求立即用新配置；会话级开关住
    # agent，不随实例更替漂移，无需重贴。
    try:
        from wing.provider.pool import reset_providers

        rebuilt = await reset_providers()
        detail = f"rebuilt {rebuilt} provider(s)"
        items.append(ReloadResultItem(name="provider", ok=True, detail=detail))
    except Exception as e:
        items.append(ReloadResultItem(name="provider", ok=False, detail=str(e)))

    try:
        for session in sm.iter_sessions():
            session.agent.context_manager.reload_skills_and_rules()
        items.append(ReloadResultItem(name="skills & rules", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="skills & rules", ok=False, detail=str(e)))

    # log level：追加在**末尾**（既有五项的名字与顺序是对外契约，probe 钉住）。
    # setup_logger 幂等（handlers.clear() 后重挂），所以「保存即生效」——
    # 这是声明层把 log.level 标成 hot 的运行期依据。
    try:
        from wing.common.logger import setup_logger

        setup_logger(level=config.log.level)
        items.append(ReloadResultItem(name="log level", ok=True))
    except Exception as e:
        items.append(ReloadResultItem(name="log level", ok=False, detail=str(e)))

    all_ok = all(item.ok for item in items)
    return ReloadResult(ok=all_ok, items=items)
