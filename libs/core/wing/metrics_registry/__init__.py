"""wing.metrics_registry — 审计注册中心。

MetricsRegistry 基于 EventBus 订阅事件，按事件类型（isinstance）分发到 handler。
handler 是纯 sink：接收事件做副作用（写文件等），不做拦截或修改。

设计约束：
  - 不复用 HookRegistry（Hook 是拦截管道，Metrics 是通知管道）
  - handler 异常不中断分发链（log error 后继续执行后续 handler）
  - 每次写入都是原子写（先写 tmp 再 rename），不丢数据
  - JSON 损坏时备份文件而非覆盖（防止历史数据丢失）
  - 安装是显式的：install() 才注册 handler 并订阅 EventBus（幂等），
    import 本模块本身无副作用
"""

from __future__ import annotations

from wing.metrics_registry.core import (
    MetricsRegistry as MetricsRegistry,  # noqa: F401 — re-export
    _atomic_write_json as _atomic_write_json,  # noqa: F401 — re-export
    _read_metrics_json as _read_metrics_json,  # noqa: F401 — re-export
    metrics_registry,
)

_installed = False


def install() -> None:
    """显式安装审计订阅（幂等）——由组合根调用（``WingRuntime.__init__``）。

    - 导入 handler 模块，触发 ``@metrics_registry.on(...)`` 装饰器注册；
    - 订阅 EventBus（``metrics_registry.handle``）。

    顶层 ``wing/__init__`` 不再在 import 期代劳。重复调用是 no-op——
    EventBus 的 subscribe 是 append 语义，重复订阅会让同一事件被处理两次
    （指标双写），因此必须幂等。
    """
    global _installed
    if _installed:
        return

    from wing.event_bus import event_bus

    # import handler 模块（幂等），触发 @metrics_registry.on(...) 装饰器注册。
    from wing.metrics_registry import (  # noqa: F401
        _compact_metrics,
        _llm_metrics,
        _tool_call_metrics,
    )

    event_bus.subscribe(metrics_registry.handle)
    _installed = True
