# wing/provider/errors.py
"""Provider 层错误类型与 HTTP 错误面处理。

模型 API 的错误详情（max_tokens 超限、thinking 签名无效等）位于响应 body——
裸 raise_for_status() 会丢弃 body，生产故障无从诊断。raise_with_body()
先读 body 再抛 ProviderHTTPError（body 摘要进异常消息，全量进日志）。
"""

from __future__ import annotations

import httpx

from wing.common.logger import log

_BODY_SNIPPET_LIMIT = 1000


class ProviderHTTPError(Exception):
    """模型 API 返回 4xx/5xx（携带响应 body）。"""

    def __init__(self, status_code: int, url: str, body: str) -> None:
        self.status_code = status_code
        self.url = url
        self.body = body
        snippet = body[:_BODY_SNIPPET_LIMIT] if body else "<empty body>"
        super().__init__(f"LLM API error {status_code} for url {url}: {snippet}")


class ProviderStreamError(Exception):
    """流中 error 事件（overloaded / rate limit / 中途终止等）。

    抛出后 with_retry 触发重试；重试耗尽经 loop 错误路径上报——截断轮次
    绝不作为成功 turn 提交。
    """

    def __init__(self, data: dict) -> None:
        self.data = data
        err = data.get("error") or {}
        super().__init__(f"LLM stream error: {err.get('message') or data}")


async def raise_with_body(resp: httpx.Response) -> None:
    """4xx/5xx 时先读 body 再抛 ProviderHTTPError；2xx/3xx 直通。"""
    if not resp.is_error:
        return
    await resp.aread()
    log.error(f"LLM API error {resp.status_code} url={resp.url} body={resp.text}")
    raise ProviderHTTPError(resp.status_code, str(resp.url), resp.text)
