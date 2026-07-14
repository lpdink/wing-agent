# wing_gateway/routes/system.py — 系统级 HTTP 端点

"""系统级查询端点：commands、models、agents。

这些端点不依赖 session，提供全局配置数据的 HTTP 访问。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from fastapi import APIRouter, Request

from wing.event import CommandInfo
from wing.gateway.protocol import AgentsResponse, CommandsResponse, ModelsResponse
from wing.magic_command.registry import magic_registry
from wing.openai_provider import OpenAIProvider

if TYPE_CHECKING:
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
    """获取所有已注册的 magic command 信息列表。

    用于输入框 popup 候选。不依赖 session。
    """
    commands = [
        CommandInfo(
            name=cmd.name,
            aliases=cmd.aliases,
            description=cmd.description,
            params=cmd.params,
        )
        for cmd in magic_registry.list_all()
    ]
    return CommandsResponse(commands=commands)


@router.get(
    "/api/models",
    response_model=ModelsResponse,
    summary="获取可用模型列表",
)
async def list_models(request: Request) -> ModelsResponse:
    """获取当前配置下可用的 LLM 模型列表。

    临时创建 OpenAIProvider 实例调用 list_models()。
    调用失败时返回空列表。不依赖 session。
    """
    try:
        provider = OpenAIProvider()
        models = await provider.list_models()
    except Exception:
        models = []
    return ModelsResponse(models=models)


@router.get(
    "/api/agents",
    response_model=AgentsResponse,
    summary="获取可用 agent 模板列表",
)
async def list_agents(request: Request) -> AgentsResponse:
    """获取所有已配置的 agent 模板名称和默认模板。

    不依赖 session。
    """
    server = _get_server(request)
    tm = server.runtime.template_manager
    return AgentsResponse(
        agents=tm.all_names,
        default_agent=tm.default_name,
    )
