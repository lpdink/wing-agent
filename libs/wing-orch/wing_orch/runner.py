"""GoalRunner — asyncio 事件循环驱动 Goal 编排。

职责：启动 ToolHost → 创建 session → 订阅 WS 事件 → 状态机驱动 → 持久化。
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import signal
import tempfile
from typing import Any
from urllib.parse import quote

import httpx
import websockets

from wing_sdk.host import WS_MAX_SIZE, ConnectionClosed, ToolHost
from wing_sdk.http_client import GatewayClient
from wing_orch.goal import (
    DEFAULT_CHECKER_SYSTEM_PROMPT,
    GoalAction,
    GoalPhase,
    GoalRole,
    GoalState,
)
from wing_sdk.tools import register_standard_tools

logger = logging.getLogger("wing-orch")


class GoalRunner:
    """Goal 编排 runner。"""

    def __init__(
        self,
        goal_prompt: str,
        gateway_url: str = "http://127.0.0.1:32523",
        api_key: str | None = None,
        client_id: str = "wing-orch",
        workspace: str = ".",
        max_rounds: int = 0,
        executor_model: str | None = None,
        checker_model: str | None = None,
        executor_tools: list[str] | None = None,
        checker_tools: list[str] | None = None,
        executor_append_system_prompt: str | None = None,
        checker_append_system_prompt: str | None = None,
        checker_system_prompt: str | None = None,
        state_file: str = ".wing-orch.json",
        resume: bool = False,
    ) -> None:
        self.goal_prompt = goal_prompt
        self.gateway_url = gateway_url.rstrip("/")
        self.api_key = api_key
        self.client_id = client_id
        self.workspace = os.path.abspath(workspace)
        self.max_rounds = max_rounds
        self.executor_model = executor_model
        self.checker_model = checker_model
        self.executor_tools = executor_tools
        self.checker_tools = checker_tools
        self.executor_append_system_prompt = executor_append_system_prompt
        self.checker_append_system_prompt = checker_append_system_prompt
        self.checker_system_prompt = checker_system_prompt
        self.state_file = state_file
        self.resume = resume

        self._state: GoalState | None = None
        self._http: GatewayClient | None = None
        self._ws: Any = None
        self._close_task: Any = None  # 保持 signal handler 创建的 task 引用
        self._shutdown = False
        self.interrupted = False
        """True if shutdown was due to signal/interrupt (exit code 130)."""

    async def run(self) -> None:
        """主入口。HTTP 错误翻译为干净的 RuntimeError（CLI 层捕获退出）。"""
        self._http = GatewayClient(self.gateway_url, self.api_key)
        try:
            await self._run_inner()
        except httpx.HTTPStatusError as e:
            try:
                detail = e.response.json().get("detail", "")
            except Exception:
                detail = e.response.text[:200]
            raise RuntimeError(
                f"Gateway API error ({e.response.status_code}): {detail}"
            ) from e
        except httpx.RequestError as e:
            # 连接层错误（gateway 不可达 / 中途断开）——与 API 错误同样干净退出
            raise RuntimeError(f"Gateway connection error: {e}") from e
        finally:
            await self._http.close()

    async def _run_inner(self) -> None:
        # 1. 启动 ToolHost（后台 task）
        host = ToolHost(self.client_id, self.gateway_url, self.api_key)
        register_standard_tools(host, self.workspace)
        host_task = asyncio.create_task(self._run_host(host))

        # 等待工具注册完成（消除竞态：session 创建必须在工具注册之后）
        # 同时监听 host_task 失败（gateway 不可达时快速报错而非等满超时）
        ready_task = asyncio.create_task(host.ready.wait())
        done, _ = await asyncio.wait(
            {ready_task, host_task},
            timeout=15.0,
            return_when=asyncio.FIRST_COMPLETED,
        )
        if host_task in done:
            # host 在注册前就失败了
            ready_task.cancel()
            exc = host_task.exception()
            raise RuntimeError(f"tool host failed to start: {exc}")
        if ready_task not in done:
            ready_task.cancel()
            host_task.cancel()
            raise RuntimeError(
                f"tool host failed to register within 15s (gateway at {self.gateway_url} reachable?)"
            )

        # 2. 建立 WS 事件连接（订阅 session 事件 + 经 HTTP 发消息）
        ws_url = self.gateway_url.replace("http://", "ws://").replace(
            "https://", "wss://"
        )
        headers: dict[str, str] = {}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"

        # client_id 全局唯一（先来先得到），tool host 已占用一个，事件连接
        # 须用另一个。注意角色由 API key 决定而非 client_id：开启鉴权时
        # wing-orch 需要 admin 角色的 key（tool_runtime 仅允许注册工具）。
        event_client_id = f"{self.client_id}-events"
        ws_uri = f"{ws_url}/ws?client_id={quote(event_client_id, safe='')}"

        try:
            async with websockets.connect(
                ws_uri, additional_headers=headers, max_size=WS_MAX_SIZE
            ) as ws:
                self._ws = ws
                # 读 ConnectResponse
                first = await ws.recv()
                msg = json.loads(first)
                if msg.get("type") != "connected":
                    raise RuntimeError(f"WS connect failed: {first}")

                # 3. 初始化或恢复 goal state
                if self.resume:
                    self._state = self._load_state()
                    await self._resume_sessions(event_client_id)
                else:
                    await self._init_goal(event_client_id)

                # 4. 事件循环
                await self._event_loop(ws, event_client_id)

        except (ConnectionClosed, websockets.exceptions.ConnectionClosed):
            logger.warning("connection closed, persisting state")
        finally:
            self._persist_state()
            host_task.cancel()

    async def _run_host(self, host: ToolHost) -> None:
        """ToolHost 服务循环（后台）。"""
        try:
            await host.run()
        except Exception as e:
            logger.error(f"tool host error: {e}")
            self._shutdown = True
            # 主动关闭 event WS，使 _event_loop 的 async for 立即退出
            if self._ws:
                await self._ws.close()

    async def _init_goal(self, event_client_id: str) -> None:
        """创建 executor + checker session，初始化状态机。"""
        assert self._http is not None

        # 工具引用（远程工具 = client_id.ToolName）
        exec_tools = self.executor_tools or [
            f"{self.client_id}.{t}"
            for t in ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]
        ]
        checker_tools = self.checker_tools or [
            f"{self.client_id}.{t}" for t in ["Bash", "Read", "Glob", "Grep"]
        ]

        # 创建 executor session
        exec_agent: dict[str, Any] = {"tools": exec_tools, "yolo": True}
        if self.executor_model:
            exec_agent["model"] = self.executor_model
        if self.executor_append_system_prompt:
            exec_agent["append_system_prompt"] = self.executor_append_system_prompt

        resp = await self._http.create_session(
            workspace=self.workspace, agent=exec_agent
        )
        executor_id = resp["session_id"]
        logger.info(f"executor session: {executor_id}")

        # 初始化状态机
        self._state, actions = GoalState.new(
            executor_id,
            self.goal_prompt,
            self.checker_system_prompt or DEFAULT_CHECKER_SYSTEM_PROMPT,
        )

        # 订阅 executor 事件
        await self._http.subscribe(executor_id, event_client_id)

        # 执行初始 actions
        await self._execute_actions(actions, event_client_id)

    async def _resume_sessions(self, event_client_id: str) -> None:
        """Resume：恢复 executor session，尝试恢复 checker。

        - executor：必须恢复（持有完整对话历史）。
        - checker：尝试 resume 旧 session（gateway 未重启时有效）；
          失败则创建新 checker（gateway 重启 / session 已过期）。
        - 统一从 EXECUTOR_WORKING 重新开始当前 round。

        注意：会向 executor 重发整份 build_prompt_user()——目标文本在其历史
        中会出现两次。这是有意为之：中断可能发生在 turn 中途，重发完整目标
        重新锚定任务，代价是一条重复消息。
        """
        assert self._http is not None and self._state is not None

        # Resume executor
        await self._http.resume_session(self._state.executor_session_id)
        await self._http.subscribe(self._state.executor_session_id, event_client_id)
        logger.info(f"resumed executor: {self._state.executor_session_id}")

        # 尝试恢复 checker；失败则新建
        checker_ok = False
        if self._state.checker_session_id:
            try:
                await self._http.resume_session(self._state.checker_session_id)
                await self._http.subscribe(
                    self._state.checker_session_id, event_client_id
                )
                logger.info(f"resumed checker: {self._state.checker_session_id}")
                checker_ok = True
            except Exception:
                logger.info("old checker unavailable, creating new one")

        if not checker_ok:
            self._state.checker_session_id = None
            await self._create_checker(event_client_id)

        # 统一从 executor 重新开始当前 round
        self._state.phase = GoalPhase.EXECUTOR_WORKING
        self._state.format_retries = 0
        await self._http.send_message(
            self._state.executor_session_id,
            self._state.build_prompt_user(),
        )

    async def _event_loop(self, ws: Any, event_client_id: str) -> None:
        """WS 事件消费循环。"""
        # 注册信号处理
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, self._handle_signal)

        async for raw in ws:
            if self._shutdown:
                break

            try:
                event = json.loads(raw)
            except json.JSONDecodeError:
                continue

            event_type = event.get("type", "")
            session_id = event.get("session_id", "")

            if event_type == "turn_result":
                await self._on_turn_result(event, session_id, event_client_id)
            elif event_type == "interrupted":
                self._on_interrupted(session_id)

        self._persist_state()

    async def _on_turn_result(
        self, event: dict, session_id: str, event_client_id: str
    ) -> None:
        assert self._state is not None

        role = self._role_for_session(session_id)
        if role is None:
            return

        result = event.get("result")
        actions = self._state.on_turn_result(role, result)

        if role == GoalRole.EXECUTOR:
            logger.info(
                f"[round={self._state.round}] executor done → checker reviewing"
            )
        else:
            finish_val = result or ""
            if "<goal_finish>true" in finish_val:
                logger.info(f"[round={self._state.round}] goal completed")
            else:
                logger.info(
                    f"[round={self._state.round}] checker: not done → next round"
                )

        # max-rounds 检查
        if self.max_rounds > 0 and self._state.round > self.max_rounds:
            logger.info(f"max rounds ({self.max_rounds}) reached, exiting")
            self._shutdown = True
            return

        await self._execute_actions(actions, event_client_id)
        self._persist_state()

    def _on_interrupted(self, session_id: str) -> None:
        assert self._state is not None
        role = self._role_for_session(session_id)
        if role:
            self._state.on_interrupted(role)
            logger.info(f"session interrupted ({role.label}), exiting")
            self._shutdown = True

    async def _execute_actions(
        self, actions: list[GoalAction], event_client_id: str
    ) -> None:
        assert self._http is not None and self._state is not None

        for action in actions:
            if action.kind == "create_checker":
                await self._create_checker(event_client_id, action.system_prompt)
            elif action.kind == "send_executor":
                await self._http.send_message(
                    self._state.executor_session_id, action.content
                )
            elif action.kind == "send_checker":
                if self._state.checker_session_id:
                    await self._http.send_message(
                        self._state.checker_session_id, action.content
                    )
            elif action.kind == "goal_complete":
                logger.info(f"goal completed: {action.reason}")
                self._shutdown = True
            elif action.kind == "exit":
                self._shutdown = True
            elif action.kind == "stall":
                logger.warning(f"stall: {action.reason}")
                self._shutdown = True

    async def _create_checker(
        self, event_client_id: str, system_prompt: str = ""
    ) -> None:
        assert self._http is not None and self._state is not None

        checker_tools = self.checker_tools or [
            f"{self.client_id}.{t}" for t in ["Bash", "Read", "Glob", "Grep"]
        ]
        agent: dict[str, Any] = {
            "tools": checker_tools,
            "yolo": True,
            "system_prompt": system_prompt or self._state.checker_system_prompt,
        }
        if self.checker_model:
            agent["model"] = self.checker_model
        if self.checker_append_system_prompt:
            agent["append_system_prompt"] = self.checker_append_system_prompt

        resp = await self._http.create_session(workspace=self.workspace, agent=agent)
        checker_id = resp["session_id"]
        await self._http.subscribe(checker_id, event_client_id)

        actions = self._state.on_checker_created(checker_id)
        logger.info(f"checker session: {checker_id}")
        # create_checker 后的 actions 通常为空
        if actions:
            await self._execute_actions(actions, event_client_id)

    def _role_for_session(self, session_id: str) -> GoalRole | None:
        assert self._state is not None
        if session_id == self._state.executor_session_id:
            return GoalRole.EXECUTOR
        if session_id == self._state.checker_session_id:
            return GoalRole.CHECKER
        return None

    def _handle_signal(self) -> None:
        logger.info("signal received, shutting down")
        self._shutdown = True
        self.interrupted = True
        # 主动关闭 event WS 以 unblock async for 循环
        if self._ws:
            self._close_task = asyncio.get_running_loop().create_task(self._ws.close())

    # ── 持久化 ────────────────────────────────────────────────

    def _persist_state(self) -> None:
        if self._state is None:
            return
        data = self._state.to_dict()
        # 原子写：tmp + rename
        dir_name = os.path.dirname(os.path.abspath(self.state_file))
        fd, tmp_path = tempfile.mkstemp(dir=dir_name, suffix=".tmp")
        try:
            with os.fdopen(fd, "w") as f:
                json.dump(data, f, ensure_ascii=False, indent=2)
            os.replace(tmp_path, self.state_file)
        except Exception:
            os.unlink(tmp_path)
            raise

    def _load_state(self) -> GoalState:
        if not os.path.exists(self.state_file):
            raise FileNotFoundError(
                f"state file not found: {self.state_file} (nothing to resume)"
            )
        with open(self.state_file) as f:
            data = json.load(f)
        state = GoalState.from_dict(data)
        logger.info(f"resumed state: round={state.round}, phase={state.phase.value}")
        return state
