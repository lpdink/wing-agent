# wing/gateway/workspace/__init__.py — Workspace 文件浏览（只读）

"""Workspace 文件浏览——Gateway 下的自含子包（只读）。

异机 Web 前端（纯渲染器）经它浏览 / 读取**提供方**（网关宿主机）工作区文件：
目录列举（分页）/ 条目元数据 / 读取（流式 + Range）。语义上是「workspace provider
的本地实现」：将来远程工具宿主以同一形状暴露它所在环境的文件系统时，替换的只有
``service.py``。

自含：只依赖 stdlib + FastAPI + pydantic，不 import 会话 / runtime / 事件等任何
Gateway 其余组件——拆 Gateway 时可原样带走。挂载在 ``/api/workspace/*``，鉴权 /
RBAC / setup mode 守门由既有中间件统一覆盖（新端点对 ``tool_runtime`` 默认关闭）。
"""

from __future__ import annotations

from .routes import router as workspace_router

__all__ = ["workspace_router"]
