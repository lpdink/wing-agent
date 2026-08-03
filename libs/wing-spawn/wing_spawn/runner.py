"""SpawnRunner — orchestrate a disposable tool container + goal execution.

Two modes:

* **Sync** (default): spawn container → register → create session → send goal
  → stream the `turn_result` → tear down. Blocks until the goal finishes.
* **Async** (`--async`): submit the goal, record a `TaskState`, launch a
  detached background watcher, and return immediately with a `task_id`. The
  watcher waits for `turn_result`, persists the outcome, fires the completion
  callback, and tears the container down. The parent polls `wing-spawn status`
  / `wing-spawn list` for progress.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import signal
import subprocess
import sys
import uuid
from typing import Any
from urllib.parse import quote

import httpx
import websockets
from wing_sdk.host import WS_MAX_SIZE
from wing_sdk.http_client import GatewayClient

from wing_spawn.callback import notify as notify_callback
from wing_spawn.containers import (
    STANDARD_TOOLS,
    cleanup_container,
    spawn_tool_container,
    wait_for_tools,
)
from wing_spawn.state import (
    CANCELLED,
    COMPLETED,
    FAILED,
    RUNNING,
    TaskState,
    load,
    save,
    update_status,
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
        async_mode: bool = False,
        callback_url: str | None = None,
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
        self.async_mode = async_mode
        self.callback_url = callback_url

        self.session_id: str | None = None
        self._ws: Any = None
        self._shutdown = False

    # ── public ─────────────────────────────────────────────────

    async def run(self) -> int:
        http = GatewayClient(self.gateway_url, self.api_key)
        try:
            if self.async_mode:
                return await self._run_async(http)
            return await self._run_sync(http)
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
            if not self.async_mode and not self.keep and self.client_id:
                cleanup_container(self.client_id)

    # ── shared: prepare (spawn → register → session → send) ───

    async def _prepare(self, http: GatewayClient) -> str:
        """Spawn the container, register tools, create a session, send the goal.

        Returns the session id.
        """
        goal = self.goal_prompt.strip()
        if not goal:
            raise ValueError("empty goal prompt")

        log.info("spawning tool container '%s'", self.client_id)
        spawn_tool_container(
            self.client_id,
            image=self.image,
            gateway_url=self.gateway_url,
            tool_key=self.tool_key,
            workspace=self.workspace,
            network=self.network,
        )

        registered = await wait_for_tools(
            self.gateway_url, self.client_id, self.api_key, timeout=60.0
        )
        log.info("registered tools: %s", ", ".join(registered))

        tools = self.tools or [f"{self.client_id}.{t}" for t in STANDARD_TOOLS]
        agent: dict[str, Any] = {"tools": tools, "model": self.model}
        if self.append_system_prompt:
            agent["append_system_prompt"] = self.append_system_prompt
        resp = await http.create_session(
            workspace=self.workspace or os.environ.get("WING_WORKSPACE"),
            agent=agent,
        )
        self.session_id = resp["session_id"]
        log.info("session: %s (model=%s)", self.session_id, self.model)

        await http.send_message(self.session_id, goal)
        log.info("goal dispatched (session=%s)", self.session_id)
        return self.session_id

    # ── sync mode ──────────────────────────────────────────────

    async def _run_sync(self, http: GatewayClient) -> int:
        session_id = await self._prepare(http)

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
            await http.subscribe(session_id, event_client_id)

            loop = asyncio.get_running_loop()
            for sig in (signal.SIGINT, signal.SIGTERM):
                try:
                    loop.add_signal_handler(sig, self._handle_signal)
                except NotImplementedError:
                    pass

            return await self._event_loop(ws, session_id)

    # ── async mode ─────────────────────────────────────────────

    async def _run_async(self, http: GatewayClient) -> int:
        session_id = await self._prepare(http)

        # Record the task before launching the watcher.
        state = TaskState(
            task_id=self.client_id,
            client_id=self.client_id,
            status=RUNNING,
            prompt=self.goal_prompt.strip(),
            model=self.model,
            gateway_url=self.gateway_url,
            callback_url=self.callback_url or "",
            session_id=session_id,
        )
        save(state)

        # Launch a detached watcher process that waits for the turn_result.
        self._launch_watcher(self.client_id)

        print(f"task submitted: {self.client_id}")
        print(f"session: {session_id}")
        print(f"status:  wing-spawn status {self.client_id}")
        print("list:    wing-spawn list")
        return 0

    def _launch_watcher(self, task_id: str) -> None:
        """Spawn a detached `wing-spawn watch <task_id>` process."""
        cmd = [sys.executable, "-m", "wing_spawn.cli", "watch", task_id]
        try:
            subprocess.Popen(
                cmd,
                start_new_session=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            log.info("watcher launched for task %s", task_id)
        except Exception as e:  # noqa: BLE001
            log.error("failed to launch watcher for %s: %s", task_id, e)
            update_status(task_id, FAILED, error=f"watcher launch failed: {e}")

    # ── watcher (background process) ───────────────────────────

    async def watch(self, task_id: str) -> int:
        """Wait for a submitted task's turn_result, persist + notify + cleanup."""
        state = load(task_id)
        if state is None:
            log.error("no such task: %s", task_id)
            return 1
        if not state.session_id:
            log.error("task %s has no session", task_id)
            return 1

        http = GatewayClient(state.gateway_url, self.api_key)
        try:
            event_client_id = f"{state.client_id}-events"
            ws_url = state.gateway_url.replace("http://", "ws://").replace(
                "https://", "wss://"
            )
            headers: dict[str, str] = {}
            if self.api_key:
                headers["Authorization"] = f"Bearer {self.api_key}"
            ws_uri = f"{ws_url}/ws?client_id={quote(event_client_id, safe='')}"

            async with websockets.connect(
                ws_uri, additional_headers=headers, max_size=WS_MAX_SIZE
            ) as ws:
                first = await ws.recv()
                if json.loads(first).get("type") != "connected":
                    log.error("WS connect failed: %s", first)
                    update_status(
                        task_id, FAILED, error="WS connect failed"
                    )
                    return 1
                await http.subscribe(state.session_id, event_client_id)

                code, result, error = await self._watch_loop(ws, state)
                if code == 0:
                    update_status(task_id, COMPLETED, result=result, exit_code=0)
                else:
                    update_status(task_id, FAILED, result=result, error=error or "failed")
                await notify_callback(
                    state.callback_url, task_id, COMPLETED if code == 0 else FAILED, result
                )
                return code
        except httpx.HTTPStatusError as e:
            log.error("gateway API error: %s", e)
            update_status(task_id, FAILED, error=str(e))
            return 1
        except Exception as e:  # noqa: BLE001
            log.exception("watch failed")
            update_status(task_id, FAILED, error=str(e))
            return 1
        finally:
            await http.close()
            if not self.keep and state.client_id:
                cleanup_container(state.client_id)

    async def _watch_loop(self, ws: Any, state: TaskState) -> tuple[int, str, str]:
        deadline = asyncio.get_running_loop().time() + self.timeout
        while True:
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=30.0)
            except asyncio.TimeoutError:
                if asyncio.get_running_loop().time() >= deadline:
                    return 1, "", f"timeout after {self.timeout:.0f}s"
                # Keep polling; also refresh state so the parent sees it alive.
                s = load(state.task_id)
                if s and s.status == CANCELLED:
                    return 130, "", "cancelled"
                continue
            except websockets.exceptions.ConnectionClosed:
                return 1, "", "WS closed"

            try:
                event = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if event.get("session_id") != state.session_id:
                continue
            etype = event.get("type", "")
            if etype == "turn_result":
                result = event.get("result") or ""
                is_error = bool(event.get("is_error"))
                if is_error:
                    return 1, result, "; ".join(str(e) for e in (event.get("errors") or [])[:3])
                return 0, result, ""
            if etype == "error":
                return 1, "", str(event.get("error", event))

    # ── result helpers ─────────────────────────────────────────

    async def _event_loop(self, ws: Any, session_id: str) -> int:
        deadline = asyncio.get_running_loop().time() + self.timeout
        async for raw in ws:
            if self._shutdown:
                log.info("interrupted")
                return 130
            try:
                event = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if event.get("session_id") != session_id:
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