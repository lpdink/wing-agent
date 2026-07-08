# wing_gateway/openapi.py — OpenAPI 元数据

"""OpenAPI tags 定义，用于 Swagger UI 分组。"""

from __future__ import annotations

OPENAPI_TAGS = [
    {
        "name": "session",
        "description": "Session 管理——创建、恢复、分叉、订阅、消息发送",
    },
    {
        "name": "health",
        "description": "服务健康检查",
    },
]
