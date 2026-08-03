"""DingSender——钉钉 OpenAPI 主动发送（不依赖 sessionWebhook 时效）。

单聊走 /v1.0/robot/oToMessages/batchSend，群聊走 /v1.0/robot/groupMessages/send；
media 上传走 oapi /media/upload。全程 httpx 异步。
"""

from __future__ import annotations

import json
import logging
import mimetypes
import time
from pathlib import Path
from typing import Any

import httpx

from .router import Conversation

log = logging.getLogger("wing-dingtalk.sender")

OPENAPI = "https://api.dingtalk.com"
OAPI = "https://oapi.dingtalk.com"
# media/upload 单文件上限（钉钉内部应用 file 类型 20MB）。
MAX_UPLOAD_BYTES = 20 * 1024 * 1024


class DingSender:
    """机器人主动消息发送器。"""

    def __init__(self, client_id: str, client_secret: str) -> None:
        self._client_id = client_id
        self._client_secret = client_secret
        self._http = httpx.AsyncClient(timeout=30.0)
        self._token: str | None = None
        self._token_expire_at: float = 0.0

    async def close(self) -> None:
        await self._http.aclose()

    # ── access token ─────────────────────────────────────────

    async def _access_token(self) -> str:
        now = time.time()
        if self._token and now < self._token_expire_at:
            return self._token
        resp = await self._http.post(
            f"{OPENAPI}/v1.0/oauth2/accessToken",
            json={"appKey": self._client_id, "appSecret": self._client_secret},
        )
        resp.raise_for_status()
        data = resp.json()
        self._token = data["accessToken"]
        # 提前 5 分钟过期，避免边界失效
        self._token_expire_at = now + int(data.get("expireIn", 7200)) - 300
        return self._token

    # ── 发送 ─────────────────────────────────────────────────

    async def send_text(self, conv: Conversation, content: str) -> None:
        await self._send(conv, "sampleText", {"content": content})

    async def send_markdown(self, conv: Conversation, title: str, text: str) -> None:
        await self._send(conv, "sampleMarkdown", {"title": title, "text": text})

    async def send_file(
        self, conv: Conversation, media_id: str, file_name: str, file_type: str
    ) -> None:
        await self._send(
            conv,
            "sampleFile",
            {"mediaId": media_id, "fileName": file_name, "fileType": file_type},
        )

    async def _send(self, conv: Conversation, msg_key: str, msg_param: dict) -> None:
        token = await self._access_token()
        headers = {"x-acs-dingtalk-access-token": token}
        body: dict[str, Any] = {
            "robotCode": self._client_id,
            "msgKey": msg_key,
            "msgParam": json.dumps(msg_param, ensure_ascii=False),
        }
        if conv.kind == "2":
            url = f"{OPENAPI}/v1.0/robot/groupMessages/send"
            body["openConversationId"] = conv.open_conversation_id
        else:
            url = f"{OPENAPI}/v1.0/robot/oToMessages/batchSend"
            body["userIds"] = [conv.staff_id]
        resp = await self._http.post(url, headers=headers, json=body)
        if resp.status_code >= 400:
            log.error(
                f"send failed ({conv.address}) {msg_key}: "
                f"{resp.status_code} {resp.text[:300]}"
            )
            resp.raise_for_status()

    # ── media 上传 ────────────────────────────────────────────

    async def upload_file(self, path: Path) -> str:
        """上传文件拿 media_id（file 类型，≤20MB）。"""
        size = path.stat().st_size
        if size > MAX_UPLOAD_BYTES:
            raise ValueError(f"file too large: {size} bytes (limit {MAX_UPLOAD_BYTES})")
        token = await self._access_token()
        mime = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
        content = path.read_bytes()
        resp = await self._http.post(
            f"{OAPI}/media/upload",
            params={"access_token": token, "type": "file"},
            files={"media": (path.name, content, mime)},
        )
        resp.raise_for_status()
        data = resp.json()
        if "media_id" not in data:
            raise RuntimeError(f"upload failed: {data}")
        return data["media_id"]
