"""GatewayClient — Wing Gateway 全量 HTTP API 客户端。"""

from __future__ import annotations

from typing import Any

import httpx


class GatewayClient:
    """异步 HTTP 客户端，覆盖 Gateway 会话与系统端点。

    工具注册端点（/api/tools/register）由 ToolHost 内部处理，不在此暴露。

    Usage:
        client = GatewayClient("http://127.0.0.1:32523", api_key="secret")
        session = await client.create_session(workspace="/path")
        await client.send_message(session["session_id"], "hello")
    """

    def __init__(
        self,
        gateway_url: str = "http://127.0.0.1:32523",
        api_key: str | None = None,
    ) -> None:
        self.gateway_url = gateway_url.rstrip("/")
        headers: dict[str, str] = {}
        if api_key:
            headers["Authorization"] = f"Bearer {api_key}"
        self._client = httpx.AsyncClient(
            base_url=self.gateway_url,
            headers=headers,
            timeout=60.0,
        )

    async def close(self) -> None:
        await self._client.aclose()

    async def __aenter__(self) -> "GatewayClient":
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self.close()

    # ── Session 生命周期 ──────────────────────────────────────

    async def create_session(
        self,
        template_name: str | None = None,
        workspace: str | None = None,
        agent: dict | None = None,
        backend: str | None = None,
    ) -> dict:
        body: dict[str, Any] = {}
        if template_name:
            body["template_name"] = template_name
        if workspace:
            body["workspace"] = workspace
        if agent:
            body["agent"] = agent
        if backend:
            body["backend"] = backend
        return await self._post("/api/session/create", body)

    async def resume_session(self, session_id: str) -> dict:
        return await self._post("/api/session/resume", {"session_id": session_id})

    async def fork_session(self, source_session_id: str, target_uuid: str) -> dict:
        return await self._post(
            "/api/session/fork",
            {"source_session_id": source_session_id, "target_uuid": target_uuid},
        )

    # ── 订阅 ─────────────────────────────────────────────────

    async def subscribe(self, session_id: str, client_id: str) -> dict:
        return await self._post(
            "/api/session/subscribe",
            {"session_id": session_id},
            headers={"X-Client-Id": client_id},
        )

    async def unsubscribe(self, session_id: str, client_id: str) -> dict:
        return await self._post(
            "/api/session/unsubscribe",
            {"session_id": session_id},
            headers={"X-Client-Id": client_id},
        )

    # ── 消息 ─────────────────────────────────────────────────

    async def send_message(
        self,
        session_id: str,
        content: str,
        tool_call_id: str | None = None,
    ) -> dict:
        body: dict[str, Any] = {"session_id": session_id, "content": content}
        if tool_call_id:
            body["tool_call_id"] = tool_call_id
        return await self._post("/api/session/send", body)

    # ── 操作 ─────────────────────────────────────────────────

    async def interrupt_session(self, session_id: str) -> dict:
        return await self._post("/api/session/interrupt", {"session_id": session_id})

    async def compact_session(
        self, session_id: str, instruction: str | None = None
    ) -> dict:
        """压缩 session 上下文。

        instruction: 可选的压缩侧重指令（附加到压缩 prompt），如
            "保留架构决策与未完成的 TODO"。
        """
        body: dict = {"session_id": session_id}
        if instruction:
            body["instruction"] = instruction
        return await self._post("/api/session/compact", body)

    async def rewind_session(self, session_id: str, target_uuid: str) -> dict:
        return await self._post(
            "/api/session/rewind",
            {"session_id": session_id, "target_uuid": target_uuid},
        )

    async def update_session(
        self,
        session_id: str,
        model: str | None = None,
        agent: str | None = None,
        title: str | None = None,
        thinking: bool | None = None,
        reasoning_effort: str | None = None,
        yolo: bool | None = None,
        workspace: str | None = None,
    ) -> dict:
        body: dict[str, Any] = {"session_id": session_id}
        for key, value in [
            ("model", model),
            ("agent", agent),
            ("title", title),
            ("thinking", thinking),
            ("reasoning_effort", reasoning_effort),
            ("yolo", yolo),
            ("workspace", workspace),
        ]:
            if value is not None:
                body[key] = value
        return await self._post("/api/session/update", body)

    # ── 查询 ─────────────────────────────────────────────────

    async def list_sessions(self) -> dict:
        return await self._get("/api/session/list")

    async def get_session(self, session_id: str) -> dict:
        return await self._get("/api/session/get", params={"session_id": session_id})

    async def get_session_info(self, session_id: str) -> dict:
        return await self._get("/api/session/info", params={"session_id": session_id})

    async def get_branches(self, session_id: str) -> dict:
        return await self._get(
            "/api/session/branches", params={"session_id": session_id}
        )

    async def health(self) -> dict:
        return await self._get("/api/health")

    # ── 系统 ─────────────────────────────────────────────────

    async def get_commands(self) -> dict:
        return await self._get("/api/commands")

    async def get_models(self) -> dict:
        return await self._get("/api/models")

    async def get_agents(self) -> dict:
        return await self._get("/api/agents")

    async def reload(self) -> dict:
        return await self._post("/api/system/reload", {})

    async def shutdown(self) -> dict:
        return await self._post("/api/shutdown", {})

    # ── 内部 ─────────────────────────────────────────────────

    async def _post(
        self, path: str, body: dict, headers: dict[str, str] | None = None
    ) -> dict:
        resp = await self._client.post(path, json=body, headers=headers)
        resp.raise_for_status()
        return resp.json()

    async def _get(self, path: str, params: dict | None = None) -> dict:
        resp = await self._client.get(path, params=params)
        resp.raise_for_status()
        return resp.json()
