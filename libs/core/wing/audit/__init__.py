"""wing/audit — 审计：事件 → ``wing.db`` 的 ``audit_*`` 追加行。

订阅 EventBus（``install()``，幂等、由组合根显式调用），按事件类型分发到
``AuditRecorder`` 的四条 INSERT（LLM 调用 / 工具调用 / 压缩 / 用户消息）。事件是**事实**，审计是它的旁路投影：不拦截、不修改、
不阻塞（同步写一行，微秒级）。

刻意删掉的旧实现（``MetricsRegistry`` + ``metrics.json`` 读整份 / 聚合 / 原子重写）：
每条事件两次全量 JSON 读写，全部在事件循环线程上，写放大随文件增长——它给流式输出
加延迟，换来的只是「按天聚合」这一种视图。

DB 打不开时整个审计静默降级（``wing.db`` 的 ``ready=False``），网关照常工作。
"""

from __future__ import annotations

from wing.audit.recorder import AuditRecorder

__all__ = ["AuditRecorder", "install"]

_installed = False


def install() -> None:
    """显式安装审计订阅（幂等）——由组合根调用（``WingRuntime.__init__``）。

    必须幂等：``EventBus.subscribe`` 是 append 语义，重复订阅会让同一事件被写两遍
    （审计双写）。顶层 ``wing/__init__`` 不在 import 期代劳。
    """
    global _installed
    if _installed:
        return

    from wing.event_bus import event_bus

    event_bus.subscribe(AuditRecorder().on_event)
    _installed = True
