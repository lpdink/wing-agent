# wing/gateway/protocol/errors.py — 统一错误形状

"""错误响应形状——前后端错误形状的唯一约定。

``ErrorResponse`` + ``error_response()``：gateway 的 exception handler 与鉴权
中间件共用，保证路由异常、请求校验失败、鉴权拒绝都输出同一形状。
"""

from __future__ import annotations

from collections.abc import Mapping

from pydantic import BaseModel, Field
from starlette.responses import JSONResponse


class ErrorResponse(BaseModel):
    """错误响应。"""

    error: str = Field(description="错误类型")
    detail: str | None = Field(default=None, description="错误详细信息")
    session_id: str | None = Field(default=None, description="关联的 session ID")
    uuid: str | None = Field(default=None, description="关联的消息 UUID")


# ErrorResponse 是前后端错误形状的唯一约定，error_response() 是它的唯一序列化
# 出口——gateway 的 exception handler 与鉴权中间件共用，保证路由异常、请求校验
# 失败、鉴权拒绝都输出同一形状，wing-api-client 因此总能结构化解析。
# 仅收录实际会被产生的状态码，避免出现误导性的死映射项。
HTTP_ERROR_TYPES: dict[int, str] = {
    400: "bad_request",
    401: "unauthorized",
    403: "forbidden",
    404: "not_found",
    405: "method_not_allowed",
    422: "validation_error",
    500: "internal_error",
    504: "gateway_timeout",
}


def _ascii_safe(text: str) -> str:
    """把错误文案里 **UTF-8 编不了** 的字符转义成 ``\\udXXX``（恒可编码）。

    错误文案常常回显用户输入（session id / 字段名 / 路径）。``JSONResponse`` 用
    ``ensure_ascii=False`` 序列化，原始代理字符会让**响应本身**炸掉——一个本该
    400 / 404 的错误就变成 500，还掩盖了真正的原因。所有错误响应都经
    :func:`error_response` 这一个出口，因此在这里兜底一次即可。
    """
    try:
        text.encode("utf-8")
    except UnicodeEncodeError:
        return text.encode("utf-8", "backslashreplace").decode("utf-8")
    return text


def error_response(
    status_code: int,
    detail: str | None = None,
    *,
    error: str | None = None,
    headers: Mapping[str, str] | None = None,
) -> JSONResponse:
    """构造统一形状（ErrorResponse）的错误响应。"""
    body = ErrorResponse(
        error=error or HTTP_ERROR_TYPES.get(status_code, "error"),
        detail=None if detail is None else _ascii_safe(detail),
    )
    return JSONResponse(
        status_code=status_code,
        content=body.model_dump(exclude_none=True),
        headers=headers,
    )
