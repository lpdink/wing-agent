# wing/gateway/routes/wake.py — 唤醒端点

"""会话唤醒的 HTTP 端点（创建 / 列表 / 取消）。

route handler 只做「参数校验 → 调 runtime.wake → 投影」。错误映射沿用既有惯例：

- ``LookupError`` → 404（源 / 目标会话不存在或已归档；cancel 的 id 不存在）；
- ``ValueError`` → 400（kind / spec / on_events / 自指 / times 组合非法）；
- ``WakeUnavailableError`` → 503（``wing.db`` 不可用，唤醒整体未武装）。

产品语义见 ``docs/dev/wake.md``。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from fastapi import APIRouter, HTTPException, Query, Request

from wing.gateway.protocol import (
    WakeCancelRequest,
    WakeCancelResponse,
    WakeCreateRequest,
    WakeCreateResponse,
    WakeListResponse,
)
from wing.gateway.projection import build_wake_info
from wing.wake import WakeUnavailableError

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

router = APIRouter(tags=["wake"])


def _get_server(request: Request) -> GatewayServer:
    return request.app.state.server


@router.post(
    "/api/wake/create",
    response_model=WakeCreateResponse,
    summary="注册一条唤醒触发器",
)
async def create_wake(body: WakeCreateRequest, request: Request) -> WakeCreateResponse:
    """注册唤醒：``session`` 型订阅 source 的事件，``timer`` 型按 spec 定时。

    创建期校验（不水合任何会话）：source / target 都必须可解析、不得自指，
    ``--in/--at`` 类一次性 spec 不接受 ``times``。
    """
    server = _get_server(request)
    try:
        trigger = server.runtime.wake.create(
            kind=body.kind,
            target_session_id=body.target_session_id,
            source_session_id=body.source_session_id,
            on_events=body.on_events,
            spec=body.spec,
            prompt=body.prompt,
            times=body.times,
            created_by="gateway",
        )
    except LookupError as exc:
        raise HTTPException(status_code=404, detail=str(exc))
    except WakeUnavailableError as exc:
        raise HTTPException(status_code=503, detail=str(exc))
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    return WakeCreateResponse(wake=build_wake_info(trigger))


@router.get(
    "/api/wake/list",
    response_model=WakeListResponse,
    summary="列出唤醒触发器",
)
async def list_wakes(
    request: Request,
    include_finished: bool = Query(False, alias="all"),
) -> WakeListResponse:
    """列出触发器（缺省只看 ``active``；``?all=true`` 含已终结的）。"""
    server = _get_server(request)
    return WakeListResponse(
        wakes=[
            build_wake_info(trigger)
            for trigger in server.runtime.wake.list_triggers(
                include_finished=include_finished
            )
        ]
    )


@router.post(
    "/api/wake/cancel",
    response_model=WakeCancelResponse,
    summary="取消一条唤醒触发器",
)
async def cancel_wake(body: WakeCancelRequest, request: Request) -> WakeCancelResponse:
    """取消触发器：``canceled=false`` 是幂等 no-op（已经终结），id 不存在才是 404。"""
    server = _get_server(request)
    try:
        canceled = server.runtime.wake.cancel(body.id)
    except LookupError as exc:
        raise HTTPException(status_code=404, detail=str(exc))
    return WakeCancelResponse(ok=True, canceled=canceled)
