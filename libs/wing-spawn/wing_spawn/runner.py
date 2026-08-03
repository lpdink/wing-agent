"""SpawnRunner — orchestrate a disposable tool container + goal execution.

Flow:
1. spawn a tool container (unique `client_id` namespace);
2. wait until the gateway sees its standard tools registered;
3. create a gateway session bound to that container's tools + model;
4. subscribe to the session and connect the WS event stream;
5. send the goal prompt;
6. wait for `turn_result`, print it, tear the container down.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import signal
import uuid
from typing import Any
from urllib.parse import quote

import httpx
import websockets
from wing_sdk.host import WS_MAX_SIZE
from wing_sdk.http_client import GatewayClient

from wing_spawn.containers import (
    STANDARD_TOOLS,
    cleanup_container,
    spawn_tool_container,
    wait_for_tools,
)

log = logging.getLogger("wing-spawn")

DEFAULT_GATEWAY = os.environ.get("WING_GATEWAY_URL", "http://gateway:32523")
DEFAULT_MODEL = os.environ.get("DEFAULT_MODEL", "deepseek-v4-flash-0731")


class SpawnRunner:
    """Run a goal against a freshly spawned tool container."""

    def __init__(
        self,
        goal_prompt: str,
        *,
        gateway_url: str = DEFAULT_GATEWAY,
        api_key: str | None = None,
        tool_key: str | None = None,
        client_id: str | None = None,
        image: str | None = None,
        workspace: str | None = None,
        network: str | None = None,
        model: str | None = None,
        tools: list[str] | None = None,
        append_system_prompt: str | None = None,
        timeout: float = 1800.0,
        keep: bool = False,
    ) -> None:
        self.goal_prompt = goal_prompt
        self.gateway_url = gateway_url.rstrip("/")
        self.api_key = api_key
        self.tool_key = tool_key
        self.client_id = client_id or f"task-{uuid.uuid4().hex[:8]}"
        self.image = image
        self.workspace = workspace
        self.network = network
        self.model = model or DEFAULT_MODEL
        self.tools = tools
        self.append_system_prompt = append_system_prompt
        self.timeout = timeout
        self.keep = keep

        self.session_id: str | None = None
        self._ws: Any = None
        self._shutdown = False

    # ── public ─────────────────────────────────────────────────

    async def run(self) -> int:
        http = GatewayClient(self.gateway_url, self.api_key)
        try:
            return await self._run_inner(http)
        except httpx.HTTPStatusError as e:
            detail = ""
            try:
                detail = e.response.json().get("detail", "") or ""
            except Exception:
                pass
            log.error("gateway API error (%s): %s", e.response.status_code, detail)
            return 1
        except httpx.RequestError as e:
            log.error("gateway connection error: %s", e)
            return 1
        except TimeoutError as e:
            log.error("%s", e)
            return 1
        except Exception:  # noqa: BLE001
            log.exception("unexpected error")
            return 1
        finally:
            await http.close()
            if not self.keep and self.client_id:
                cleanup_container(self.client_id)

    # ── internals ──────────────────────────────────────────────

    async def _run_inner(self, http: GatewayClient) -> int:
        goal = self.goal_prompt.strip()
        if not goal:
            log.error("empty goal prompt")
            return 1

        # 1. Spawn the tool container.
        log.info("spawning tool container '%s'", self.client_id)
        spawn_tool_container(
            self.client_id,
            image=self.image,
            gateway_url=self.gateway_url,
            tool_key=self.tool_key,
            workspace=self.workspace,
            network=self.network,
        )

        # 2. Wait for its tools to register.
        try:
            registered = await wait_for_tools(
                self.gateway_url, self.client_id, self.api_key, timeout=60.0
            )
        except TimeoutError:
            log.error("container never registered its tools; aborting")
            return 1
        log.info("registered tools: %s", ", ".join(registered))

        # 3. Create the session bound to the container's tools.
        tools = self.tools or [
            f"{self.client_id}.{t}" for t in STANDARD_TOOLS
        ]
        agent: dict[str, Any] = {"tools": tools, "model": self.model}
        if self.append_system_prompt:
            agent["append_system_prompt"] = self.append_system_prompt
        resp = await http.create_session(
            workspace=self.workspace or os.environ.get("WING_WORKSPACE"),
            agent=agent,
        )
        self.session_id = resp["session_id"]
        log.info("session: %s (model=%s, tools=%s)", self.session_id, self.model, tools)

        # 4. Connect the WS event stream, then subscribe (the gateway requires
        #    the client to be connected before it accepts a subscription).
        event_client_id = f"{self.client_id}-events"

        ws_url = self.gateway_url.replace("http://", "ws://").replace(
            "https://", "wss://"
        )
        headers: dict[str, str] = {}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        ws_uri = f"{ws_url}/ws?client_id={quote(event_client_id, safe='')}"

        async with websockets.connect(
            ws_uri, additional_headers=headers, max_size=WS_MAX_SIZE
        ) as ws:
            self._ws = ws
            first = await ws.recv()
            if json.loads(first).get("type") != "connected":
                log.error("WS connect failed: %s", first)
                return 1

            await http.subscribe(self.session_id, event_client_id)

            # 5. Send the goal prompt.
            loop = asyncio.get_running_loop()
            for sig in (signal.SIGINT, signal.SIGTERM):
                try:
                    loop.add_signal_handler(sig, self._handle_signal)
                except NotImplementedError:
                    pass
            await http.send_message(self.session_id, goal)
            log.info("goal dispatched; waiting for turn_result…")

            # 6. Wait for the turn result.
            return await self._event_loop(ws)

    async def _event_loop(self, ws: Any) -> int:
        deadline = asyncio.get_running_loop().time() + self.timeout
        async for raw in ws:
            if self._shutdown:
                log.info("interrupted")
                return 130
            try:
                event = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if event.get("session_id") != self.session_id:
                continue
            etype = event.get("type", "")
            if etype == "turn_result":
                return self._emit_result(event)
            if etype == "error":
                log.error("session error: %s", event.get("error", event))
                return 1
            if asyncio.get_running_loop().time() >= deadline:
                log.error("timeout after %.0fs waiting for turn_result", self.timeout)
                return 1
        log.info("WS closed before turn_result")
        return 1

    def _emit_result(self, event: dict) -> int:
        result = event.get("result") or ""
        is_error = bool(event.get("is_error"))
        errors = event.get("errors") or []
        num_turns = event.get("num_turns", 0)

        if is_error:
            prefix = f"[error] turn {num_turns}"
            if errors:
                prefix += f" ({'; '.join(str(e) for e in errors[:3])})"
            print(f"{prefix}\n{result}")
            return 1
        print(f"[done] {num_turns} turns")
        print(result)
        return 0

    def _handle_signal(self) -> None:
        self._shutdown = True
        if self._ws:
            asyncio.create_task(self._ws.close())