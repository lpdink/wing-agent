# wing/system.py
"""wing/system.py — 系统级热重载流程。

``reload_system(sm)`` 是 ``/api/system/reload`` 的实现体：config → hooks → prompt
commands → provider → skills & rules → log level，逐项独立 try/except；config 失败立即中止
（后续项不再尝试），其余项失败继续——**步骤顺序与逐项 detail 是对外契约**
（probe ``test_system_reload`` 抓名字序与逐项 ok）。config 项内部还包含一步
``sm.reload_templates()``（模板管理器跟随新 config 重建，见 ``SessionManager.reload_templates``）。
``log level`` 是**末项**（其余五项的名字与顺序不许动）：``setup_logger`` 幂等重挂
handler，把声明的 ``log.level: hot`` 兑现到运行期。

``ReloadResult`` / ``ReloadResultItem`` 与流程体住这里，``WingRuntime.reload_system()``
只保留一行委托。函数体内的 import 保持在函数体内（懒加载）——``wing.config.load_config``
等名字在调用时刻解析，保持可 monkeypatch（``tests/test_runtime_session_ops.py``）。
"""

from __future__ import annotations

from collections.abc import Callable
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


def record_step(
    items: list[ReloadResultItem], name: str, fn: Callable[[], None]
) -> bool:
    """跑一步 reload 并记一条 :class:`ReloadResultItem`；失败返回 ``False``。

    ``fn`` 抛异常即记 ``ok=False`` + ``str(e)``，否则记 ``ok=True``。**中止还是继续
    由调用方决定**——``system.reload_system`` 除 config 外都继续，
    ``GatewayServer._enter_operational`` 任一步失败即中止；那行 ``if not record_step(...)``
    就是策略所在。

    本函数不包住三类步骤：产出值（``load_config`` 的 ``Config``、``WingRuntime()``
    的实例）、需要 ``await``（``reset_providers()``）、以及自带 ``detail`` 的
    （``_warn_auth_lockout`` 的告警文案）——它们写成显式 ``try`` / 显式 append
    更能看清输出去哪。
    """
    try:
        fn()
    except Exception as e:
        items.append(ReloadResultItem(name=name, ok=False, detail=str(e)))
        return False
    items.append(ReloadResultItem(name=name, ok=True))
    return True


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

    def _reload_hooks() -> None:
        hooks.clear()
        load_hooks(config.hooks)

    record_step(items, "hooks", _reload_hooks)

    def _reload_commands() -> None:
        magic_registry.remove_by_source("prompt")
        register_prompt_commands(config.commands.paths)

    record_step(items, "prompt commands", _reload_commands)

    # provider 重建：共享池按新配置重建全部实例（先建后换——任一构建失败池保持
    # 原样）；旧实例退场（在途请求跑完后自动关闭，reload 对会话透明）。会话不再
    # 逐一重建：实例经池按 name 实时解析，新请求立即用新配置；会话级开关住
    # agent，不随实例更替漂移，无需重贴。
    # provider 是唯一需要 await 的一步，因此不走 `record_step`（它只接同步 fn）。
    try:
        from wing.provider.pool import reset_providers

        rebuilt = await reset_providers()
    except Exception as e:
        items.append(ReloadResultItem(name="provider", ok=False, detail=str(e)))
    else:
        items.append(
            ReloadResultItem(
                name="provider", ok=True, detail=f"rebuilt {rebuilt} provider(s)"
            )
        )

    def _reload_skills() -> None:
        for session in sm.iter_sessions():
            session.agent.context_manager.reload_skills_and_rules()

    record_step(items, "skills & rules", _reload_skills)

    # log level：追加在**末尾**（既有五项的名字与顺序是对外契约，probe 钉住）。
    # setup_logger 幂等（handlers.clear() 后重挂），所以「保存即生效」——
    # 这是声明层把 log.level 标成 hot 的运行期依据。
    def _reload_log_level() -> None:
        from wing.common.logger import setup_logger
        from wing.request_context import get_request_context

        # context= 必须重传：日志的 session / request 关联段由 provider 逐条现取，
        # 而 setup_logger 会 handlers.clear() 后重挂——不传就等于热重载把关联段抹掉。
        setup_logger(level=config.log.level, context=get_request_context)

    record_step(items, "log level", _reload_log_level)

    all_ok = all(item.ok for item in items)
    return ReloadResult(ok=all_ok, items=items)
