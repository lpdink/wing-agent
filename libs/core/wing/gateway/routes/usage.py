# wing/gateway/routes/usage.py — 用量聚合端点
"""``GET /api/usage``：把审计流水 + 会话目录投影成「同一组度量 × 一个维度」。

路由只做「校验 → 调 runtime → 构造响应」：SQL 与聚合全在 ``wing.metrics``
（``server.runtime.usage_report`` 是薄转发），整条路径**只读库**——不水合会话、
不发事件、不改任何状态（已被逐出的会话不会因为这个端点被读回内存）。

错误语义与其它端点一致：**语义非法 400**（未知 ``group_by`` / 日期格式 / ``since >
until`` / ``tree`` 没配 ``session_id`` / 会话 id 词法非法），**未知会话 404**，
**形态非法 422**（``limit`` 越界由 ``Query`` 约束给出）。空数据永远 200。
"""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

from fastapi import APIRouter, HTTPException, Query, Request

from wing.gateway.protocol import UsageResponse
from wing.metrics import GROUP_BY_VALUES, MAX_LIMIT

from ._common import get_server

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

router = APIRouter(tags=["usage"])

_GROUP_BY_TEXT = ", ".join(GROUP_BY_VALUES)


@router.get(
    "/api/usage",
    response_model=UsageResponse,
    summary="用量聚合（同一组度量 × 一个维度）",
)
async def usage(
    request: Request,
    since: str | None = Query(
        None,
        description=(
            "窗口起点：`YYYY-MM-DD` 或字面量 `all`（= 有记录的第一天）；"
            "缺省 = until 往前 84 天"
        ),
    ),
    until: str | None = Query(
        None, description="窗口终点（含）：`YYYY-MM-DD`；缺省 = 今天（网关本地时区）"
    ),
    group_by: str = Query("day", description=f"维度：{_GROUP_BY_TEXT}"),
    session_id: str | None = Query(
        None, description="把范围收敛到某会话（`tree=true` 时再带上全部子孙）"
    ),
    tree: bool = Query(False, description="包含子孙会话（必须配 `session_id`）"),
    limit: int = Query(
        20,
        ge=1,
        le=MAX_LIMIT,
        description="排行类维度（model / session / tool / workspace / agent）的行数上限",
    ),
) -> UsageResponse:
    return await asyncio.to_thread(
        _usage_response,
        get_server(request),
        since=since,
        until=until,
        group_by=group_by,
        session_id=session_id,
        tree=tree,
        limit=limit,
    )


def _usage_response(
    server: GatewayServer,
    *,
    since: str | None,
    until: str | None,
    group_by: str,
    session_id: str | None,
    tree: bool,
    limit: int,
) -> UsageResponse:
    """端点的工作体（线程池执行）：读库 + 聚合 → 强类型响应。

    读库（含聚合）留在循环线程上会让所有会话的流式帧排队——与缓存账本端点同理，
    整笔工作进线程池。
    """
    try:
        report = server.runtime.usage_report(
            since=since,
            until=until,
            group_by=group_by,
            session_id=session_id,
            tree=tree,
            limit=limit,
        )
    except LookupError as e:
        raise HTTPException(status_code=404, detail=str(e)) from None
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e)) from None
    return UsageResponse(**report)
