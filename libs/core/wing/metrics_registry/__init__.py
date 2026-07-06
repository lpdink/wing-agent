"""wing.metrics_registry — 审计注册中心。

MetricsRegistry 基于 EventBus 订阅事件，按事件类型（isinstance）分发到 handler。
handler 是纯 sink：接收事件做副作用（写文件等），不做拦截或修改。

设计约束：
  - 不复用 HookRegistry（Hook 是拦截管道，Metrics 是通知管道）
  - handler 异常不中断分发链（log error 后继续执行后续 handler）
  - 每次写入都是原子写（先写 tmp 再 rename），不丢数据
  - JSON 损坏时备份文件而非覆盖（防止历史数据丢失）
  - 模块加载时即订阅 EventBus（单例模式，避免多次 subscribe）
"""

from __future__ import annotations

from wing.metrics_registry.core import (
    MetricsRegistry as MetricsRegistry,  # noqa: F401 — re-export
    _atomic_write_json as _atomic_write_json,  # noqa: F401 — re-export
    _read_metrics_json as _read_metrics_json,  # noqa: F401 — re-export
    metrics_registry,
)

# 模块加载时即订阅 EventBus — 单例模式保证只执行一次。
from wing.event_bus import event_bus  # noqa: E402

# import handler 模块，触发 @metrics_registry.on(...) 装饰器注册。
from wing.metrics_registry._llm_metrics import (  # noqa: F401
    _handle_global_metrics,
    _handle_session_metrics,
)
from wing.metrics_registry._tool_call_metrics import (  # noqa: F401
    _handle_tool_call_global,
    _handle_tool_call_session,
)
from wing.metrics_registry._compact_metrics import (  # noqa: F401
    _handle_compact_global,
    _handle_compact_session,
)
from wing.metrics_registry.experimental import (  # noqa: F401
    _handle_better_edit_experiment,
)

event_bus.subscribe(metrics_registry.handle)
