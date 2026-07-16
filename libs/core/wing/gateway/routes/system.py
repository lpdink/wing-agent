# wing_gateway/routes/system.py — 系统级 HTTP 端点

"""系统级查询端点：commands、models、agents。

这些端点不依赖 session，提供全局配置数据的 HTTP 访问。
"""

from __future__ import annotations

import asyncio
import os
import signal
from typing import TYPE_CHECKING

from fastapi import APIRouter, Request

from wing.event import CommandInfo
from wing.gateway.protocol import (
    AgentsResponse,
    CommandsResponse,
    ModelsResponse,
    ReloadResponse,
    ReloadResultItem,
)
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
    """获取所有已注册的 prompt 类型命令信息列表。

    只返回 source='prompt' 的命令（用户定义的 .md 命令）。
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
        if cmd.source == "prompt"
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


@router.post(
    "/api/system/reload",
    response_model=ReloadResponse,
    summary="热重载全局配置",
)
async def reload_system(request: Request) -> ReloadResponse:
    """热重载 config.yaml、hooks、prompt commands、OpenAI provider、skills & rules。

    config 加载失败时立即中止。其余项失败时继续重载剩余项，
    返回每项的成功/失败详情。
    """
    from wing.config import load_config, load_hooks
    from wing.hook_registry import hooks
    from wing.magic_command.prompt_commands import register_prompt_commands

    server = _get_server(request)
    results: list[ReloadResultItem] = []
    total = 5
    success = 0

    # 1. Reload config
    try:
        config = load_config(reload=True)
        results.append(ReloadResultItem(name="config.yaml", ok=True))
        success += 1
    except Exception as e:
        results.append(ReloadResultItem(name="config.yaml", ok=False, detail=str(e)))
        return ReloadResponse(ok=False, results=results)

    # 2. Reload hooks
    try:
        hooks.clear()
        load_hooks(config.hooks)
        results.append(ReloadResultItem(name="hooks", ok=True))
        success += 1
    except Exception as e:
        results.append(ReloadResultItem(name="hooks", ok=False, detail=str(e)))

    # 3. Reload prompt commands
    try:
        magic_registry.remove_by_source("prompt")
        register_prompt_commands(config.commands.paths)
        results.append(ReloadResultItem(name="prompt commands", ok=True))
        success += 1
    except Exception as e:
        results.append(
            ReloadResultItem(name="prompt commands", ok=False, detail=str(e))
        )

    # 4. Reload OpenAI provider — 使用第一个 session 的 provider
    try:
        first_session = next(iter(server.runtime.sm._sessions.values()), None)
        if first_session:
            changes = first_session.agent.model_provider.reload()
            detail = ", ".join(changes) if changes else "unchanged"
            results.append(ReloadResultItem(name="provider", ok=True, detail=detail))
        else:
            results.append(
                ReloadResultItem(name="provider", ok=True, detail="no active session")
            )
        success += 1
    except Exception as e:
        results.append(ReloadResultItem(name="provider", ok=False, detail=str(e)))

    # 5. Reload skills & rules for all active sessions
    try:
        for session in server.runtime.sm._sessions.values():
            session.agent.context_manager.reload_skills_and_rules()
        results.append(ReloadResultItem(name="skills & rules", ok=True))
        success += 1
    except Exception as e:
        results.append(ReloadResultItem(name="skills & rules", ok=False, detail=str(e)))

    return ReloadResponse(ok=(success == total), results=results)


@router.post(
    "/api/shutdown",
    summary="优雅关闭 Gateway",
)
async def shutdown() -> dict[str, str]:
    """优雅关闭 Gateway 进程。

    先返回 HTTP 200，再延迟 0.1s 向自身发送 SIGTERM。
    uvicorn 收到 SIGTERM 后优雅关闭（drain 现有连接）。
    """
    asyncio.get_running_loop().call_later(0.1, os.kill, os.getpid(), signal.SIGTERM)
    return {"status": "shutting_down"}
