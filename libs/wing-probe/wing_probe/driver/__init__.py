"""driver 子包：经公开 HTTP / WS 协议驱动网关的"假用户"。

- ``http.py``：``DriverHttp``（复用 ``wing_sdk.GatewayClient``）+ 调用留档 + 非 2xx 报错；
- ``ws.py``：``GatewayWS``（16 MiB 帧上限 + ``_chunk`` 信封重组 + 原始帧日志）；
- ``session.py``：``Driver``（连接 + 会话工厂）与 ``Session``（会话动作 + 时间线 + history）。
"""

from wing_probe.driver.http import (
    DEFAULT_TIMEOUT,
    DriverHttp,
    DriverHttpError,
    HttpCall,
    render_http_failure,
)
from wing_probe.driver.session import (
    DEFAULT_TURN_WITHIN,
    Driver,
    DriverError,
    EnvLike,
    Session,
    ws_url,
)
from wing_probe.driver.ws import (
    CHUNK_TYPE,
    DEFAULT_LIMITS,
    DEFAULT_MAX_SIZE,
    IDLE_TIMEOUT,
    INVALID_FRAME_TYPE,
    MAX_BUFFERED_BYTES,
    MAX_CHUNKS,
    ChunkEnvelope,
    ChunkLimits,
    Delivery,
    EventHandler,
    GatewayWS,
    Reassembler,
    ReassemblyError,
    WsError,
    is_chunk_frame,
    parse_envelope,
)

__all__ = [
    "CHUNK_TYPE",
    "DEFAULT_LIMITS",
    "DEFAULT_MAX_SIZE",
    "DEFAULT_TIMEOUT",
    "DEFAULT_TURN_WITHIN",
    "IDLE_TIMEOUT",
    "INVALID_FRAME_TYPE",
    "MAX_BUFFERED_BYTES",
    "MAX_CHUNKS",
    "ChunkEnvelope",
    "ChunkLimits",
    "Delivery",
    "Driver",
    "DriverError",
    "DriverHttp",
    "DriverHttpError",
    "EnvLike",
    "EventHandler",
    "GatewayWS",
    "HttpCall",
    "Reassembler",
    "ReassemblyError",
    "Session",
    "WsError",
    "is_chunk_frame",
    "parse_envelope",
    "render_http_failure",
    "ws_url",
]
