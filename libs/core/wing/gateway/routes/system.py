# wing_gateway/routes/system.py — 系统级 HTTP 端点

"""系统级端点：commands、models、agents、reload、shutdown。

Route handler 只做参数验证和 HTTP 响应构造——业务逻辑在 WingRuntime 中。
"""

from __future__ import annotations

import asyncio
import os
import signal
from typing import TYPE_CHECKING

from fastapi import APIRouter, Request

from wing.config import get_config
from wing.event import CommandInfo
from wing.gateway.protocol import (
    AgentsResponse,
    CommandsResponse,
    ModelsResponse,
    ReloadResponse,
    ReloadResultItem as ReloadResultItemProto,
    ToolInfo,
    ToolsListResponse,
)
from wing.magic_command.registry import magic_registry
from wing.provider import create_provider

if TYPE_CHECKING:
    from wing.config import ProviderConfig
    from wing.gateway.server import GatewayServer

router = APIRouter(tags=["system"])


def _get_server(request: Request) -> GatewayServer:
    """从 app.state 获取 GatewayServer 实例。"""
    return request.app.state.server


@router.get(
    "/api/commands",
    response_model=CommandsResponse,
    summary="获取可用命令列表",
)
async def list_commands(request: Request) -> CommandsResponse:
    """获取所有已注册的 prompt 类型命令信息列表。"""
    commands = [
        CommandInfo(
            name=cmd.name,
            aliases=cmd.aliases,
            description=cmd.description,
            params=cmd.params,
        )
        for cmd in magic_registry.list_all()
        if cmd.source == "prompt"
    ]
    return CommandsResponse(commands=commands)


@router.get(
    "/api/models",
    response_model=ModelsResponse,
    summary="获取可用模型列表",
)
async def list_models(request: Request) -> ModelsResponse:
    """并发请求所有已配置 provider，聚合去重返回模型列表。"""
    config = get_config()

    async def _query_one(provider_cfg: "ProviderConfig") -> list[str]:
        try:
            provider = create_provider(provider_cfg)
            return await asyncio.wait_for(provider.list_models(), timeout=10.0)
        except Exception:
            return []

    results = await asyncio.gather(
        *[_query_one(p) for p in config.providers],
        return_exceptions=True,
    )
    models: set[str] = set()
    for r in results:
        if isinstance(r, list):
            models.update(r)
    return ModelsResponse(models=sorted(models))


@router.get(
    "/api/agents",
    response_model=AgentsResponse,
    summary="获取可用 agent 模板列表",
)
async def list_agents(request: Request) -> AgentsResponse:
    """获取所有已配置的 agent 模板名称和默认模板。"""
    server = _get_server(request)
    tm = server.runtime.template_manager
    return AgentsResponse(
        agents=tm.all_names,
        default_agent=tm.default_name,
    )


@router.post(
    "/api/system/reload",
    response_model=ReloadResponse,
    summary="热重载全局配置",
)
async def reload_system(request: Request) -> ReloadResponse:
    """热重载 config.yaml、hooks、prompt commands、OpenAI provider、skills & rules。"""
    server = _get_server(request)
    result = server.runtime.reload_system()
    return ReloadResponse(
        ok=result.ok,
        results=[
            ReloadResultItemProto(name=item.name, ok=item.ok, detail=item.detail)
            for item in result.items
        ],
    )


@router.post(
    "/api/shutdown",
    summary="优雅关闭 Gateway",
)
async def shutdown() -> dict[str, str]:
    """优雅关闭 Gateway 进程。"""
    asyncio.get_running_loop().call_later(0.1, os.kill, os.getpid(), signal.SIGTERM)
    return {"status": "shutting_down"}


@router.get(
    "/api/tools",
    response_model=ToolsListResponse,
    summary="获取全局工具列表",
)
async def list_tools() -> ToolsListResponse:
    """列出所有已注册工具（含内置 + 远程），平铺列表。"""
    from wing.tool_registry import ToolRef, tool_registry

    return ToolsListResponse(
        tools=[
            ToolInfo(
                ref=str(ToolRef(namespace=t.namespace, name=t.name)),
                namespace=t.namespace,
                name=t.name,
                llm_name=t.effective_llm_name,
                description=t.description,
            )
            for t in tool_registry.tools
        ]
    )
