# wing_gateway/openapi.py — OpenAPI 元数据

"""OpenAPI 元数据配置——版本、描述、标签、服务器。

所有 OpenAPI 相关的常量集中在此文件中管理。
修改版本号时，请同步更新 ``libs/core/pyproject.toml`` 中的 ``version``。
"""

from __future__ import annotations

# ---------------------------------------------------------------------------
# 版本 — 与 pyproject.toml 中的 version 保持同步
# ---------------------------------------------------------------------------
OPENAPI_VERSION = "0.1.0"

# ---------------------------------------------------------------------------
# Tags — Swagger UI 分组 + 端点标记
# ---------------------------------------------------------------------------
OPENAPI_TAGS = [
    {
        "name": "session",
        "description": "Session 生命周期管理——创建、恢复、分叉、订阅、消息发送、查询、状态变更",
    },
    {
        "name": "system",
        "description": "系统级查询——命令列表、模型列表、Agent 模板列表",
    },
    {
        "name": "health",
        "description": "服务健康检查",
    },
]

# ---------------------------------------------------------------------------
# 完整 Metadata — 供 FastAPI() 构造函数使用
# ---------------------------------------------------------------------------
OPENAPI_METADATA: dict = {
    "title": "Wing Gateway API",
    "description": (
        "Wing Agent Gateway API.\n"
        "\n"
        "## 协议\n"
        "\n"
        "Gateway 同时提供两种协议：\n"
        "\n"
        "- **HTTP**（RPC 风格）：session 管理、订阅管理、消息发送\n"
        "- **WebSocket** (`/ws`)：实时事件流（ReAct 事件、状态变更事件）\n"
        "\n"
        "## 订阅模型\n"
        "\n"
        "1. 客户端建立 WS 连接，获取 `client_id`\n"
        "2. 通过 HTTP `POST /api/session/create` 创建 session\n"
        "3. 通过 HTTP `POST /api/session/subscribe`（携带 `X-Client-Id` header）订阅 session 事件\n"
        "4. 一个 WS 连接可以订阅多个 session\n"
    ),
    "version": OPENAPI_VERSION,
    "servers": [
        {"url": "http://127.0.0.1:32523", "description": "Local development"},
    ],
    "tags": OPENAPI_TAGS,
}
