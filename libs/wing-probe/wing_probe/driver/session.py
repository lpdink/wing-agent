"""会话句柄与 driver（tasks 4.3）——驱动网关的"假用户"。

``Driver`` 是连接 + 会话工厂，``Session`` 是一个会话的操作面：

- 每个 ``Session`` 挂载一条 **事件时间线**（WS 订阅，``session.watch`` 做断言）
  与本地 **history 视图**（``session.history``，``wing_probe.history`` 提供）；
- 上行只走两条公开通道：WS ``ClientRequest`` 帧（``send`` / ``answer``）与 HTTP
  RPC（``interrupt`` / ``compact`` / ``rewind`` / ``fork`` / ``branches`` / ``info``）；
- 所有 HTTP 调用留档在 ``driver.http``，所有入站帧留档在 ``driver.frames``。

创建会话（场景轮的主要入口）：

    driver = await Driver.connect(env)
    session = await driver.session(model="probe/basic", workspace=tmp_path)
    await session.chat("hi")

``model`` / ``tools`` / ``yolo`` 等经 POST /api/session/create 的 ``agent``
（``AgentOverride``）下发，``workspace`` 走请求体同名字段。
"""

from __future__ import annotations

import time
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol

from wing_probe.driver.http import DriverHttp
from wing_probe.driver.ws import (
    DEFAULT_LIMITS,
    DEFAULT_MAX_SIZE,
    DEFAULT_OPEN_TIMEOUT,
    ChunkLimits,
    Delivery,
    GatewayWS,
)
from wing_probe.watch.expect import Watcher
from wing_probe.watch.timeline import Clock, Event, FrameLog, Timeline

if TYPE_CHECKING:
    # ``wing_probe.history``（并行轮）—— 只在类型层引用，运行期按需导入：
    # 模块缺失时 driver 仍可独立使用（history 挂载点报错并说明原因）。
    from wing_probe.history import HistoryView

#: ``chat`` 等待整轮 ReAct 结束的默认上限（比单事件 expect 宽——一轮里可能有工具）。
DEFAULT_TURN_WITHIN = 30.0


class DriverError(RuntimeError):
    """driver 用法 / 环境错误（会话不存在、响应缺字段、WS 未连接…）。"""


class EnvLike(Protocol):
    """driver 依赖的 ``ProbeEnv`` 最小面（见 ``wing_probe.env.ProbeEnv``）。"""

    @property
    def gateway_url(self) -> str: ...

    @property
    def started_at(self) -> float: ...

    @property
    def artifacts_path(self) -> Path: ...

    def session_dir(self, session_id: str) -> Path: ...


def ws_url(gateway_url: str) -> str:
    """``http(s)://host:port`` → ``ws(s)://host:port/ws``。"""
    base = gateway_url.rstrip("/")
    if base.startswith("https://"):
        base = "wss://" + base[len("https://") :]
    elif base.startswith("http://"):
        base = "ws://" + base[len("http://") :]
    return f"{base}/ws"


def _load_history_view(session_dir: Path) -> HistoryView:
    """按需构造 history 视图（函数级导入：driver 的导入不依赖 history 模块）。"""
    from wing_probe.history import HistoryView

    return HistoryView(session_dir)


class Session:
    """一个会话的操作面（动作 + 事件时间线 + history 视图）。

    由 ``Driver.session(...)`` / ``Driver.resume(...)`` / ``Session.fork(...)`` 创建，
    不要在别处手工构造（事件路由依赖 driver 的注册表）。
    """

    def __init__(
        self,
        driver: Driver,
        session_id: str,
        *,
        response: Mapping[str, Any] | None = None,
        workspace: str | Path | None = None,
    ) -> None:
        self.driver = driver
        self.session_id = session_id
        self.response: dict[str, Any] = dict(response or {})
        """创建 / 恢复 / 分叉的 HTTP 响应（``fork`` 的 ``draft`` 等字段在此）。"""
        self.workspace = Path(workspace).expanduser().resolve() if workspace else None
        self.timeline = Timeline(
            f"session {session_id}", started_at=driver.env.started_at
        )
        self.watch = Watcher(self.timeline, frames=driver.frames)
        """事件断言门面（游标模型：``expect`` / ``expect_none`` / ``assert_never`` …）。"""

    def __repr__(self) -> str:
        return f"<Session {self.session_id}>"

    # ── 挂载 ──────────────────────────────────────────────────

    @property
    def frames(self) -> FrameLog:
        """全部入站原始帧（连接级，报告与现场转储的数据源）。"""
        return self.driver.frames

    @property
    def session_dir(self) -> Path:
        """会话持久目录（``history.jsonl`` 所在）。"""
        return self.driver.env.session_dir(self.session_id)

    @property
    def history(self) -> HistoryView:
        """本地 history 视图。

        **每次访问都重新解析 ``history.jsonl``**（design D6 的"操作前后两个视图"
        就是这么用的：``before = s.history; await op(); after = s.history``）。
        """
        return _load_history_view(self.session_dir)

    # ── 上行消息 ──────────────────────────────────────────────

    async def send(self, content: str, *, tool_call_id: str | None = None) -> str:
        """经 WS 发一条用户消息（返回 ``request_id``——可对上 ``delivered`` 等事件）。

        ``tool_call_id`` 非空时是**定向答复**（resolve 对应 ask 的 feedback waiter）；
        场景里更常见的是 ``await session.answer(ask_event, "...")``。
        """
        ws = self.driver.websocket
        return await ws.send_request(
            self.session_id, content, tool_call_id=tool_call_id
        )

    async def chat(self, text: str, *, within: float = DEFAULT_TURN_WITHIN) -> Event:
        """发送消息并等到本轮 ``turn_result``（返回该事件，供断言 subtype / usage）。

        超时报告里能看到游标之后的全部事件（含 ``error``），所以"模型报错导致
        轮次没结束"这类失败不需要重跑即可定位。
        """
        await self.send(text)
        return await self.watch.expect("turn_result", within=within)

    async def answer(self, ask: Event | str, text: str) -> str:
        """定向答复一个 ask 事件（``ask`` 可以是事件对象，也可以直接给 tool_call_id）。"""
        tool_call_id = ask.data.get("tool_call_id") if isinstance(ask, Event) else ask
        if not tool_call_id:
            raise DriverError(
                "answer() needs a tool_call_id: pass the ask event (its "
                "tool_call_id field) or the id itself"
            )
        return await self.send(text, tool_call_id=str(tool_call_id))

    # ── HTTP 动作 ─────────────────────────────────────────────

    async def interrupt(self) -> dict:
        """中断当前任务（``Esc``）。"""
        return await self.driver.http.interrupt_session(self.session_id)

    async def compact(self, instruction: str | None = None) -> dict:
        """压缩上下文（``instruction`` 为可选侧重指令）。"""
        return await self.driver.http.compact_session(self.session_id, instruction)

    async def rewind(self, target_uuid: str) -> dict:
        """回退到指定消息 uuid（响应的 ``draft`` 由调用方断言）。"""
        return await self.driver.http.rewind_session(self.session_id, target_uuid)

    async def fork(self, target_uuid: str) -> Session:
        """从指定消息 uuid 分叉出新会话，返回已订阅的新 ``Session`` 句柄。"""
        response = await self.driver.http.fork_session(self.session_id, target_uuid)
        new_id = response.get("session_id")
        if not isinstance(new_id, str):
            raise DriverError(f"fork response has no session_id: {response!r}")
        return await self.driver.attach(
            new_id, response=response, workspace=self.workspace
        )

    async def branches(self) -> dict:
        """可回退 / 分叉的消息节点。"""
        return await self.driver.http.get_branches(self.session_id)

    async def info(self) -> dict:
        """运行时状态（``context_stats`` / skills / reasoning effort…）。"""
        return await self.driver.http.get_session_info(self.session_id)

    async def get(self) -> dict:
        """会话详情（元数据 + 消息）。"""
        return await self.driver.http.get_session(self.session_id)


class Driver:
    """连接（WS + HTTP）与会话工厂。

    ``Driver.connect(env)`` 建立 WS 订阅连接（拿 ``client_id``）与带留档的 HTTP
    客户端；之后每次 ``session(...)`` / ``resume(...)`` / ``attach(...)`` 都把新会话
    注册进事件路由表**并**经 ``/api/session/subscribe`` 订阅（重放的 ``sync_session``
    因此不会丢——注册发生在订阅之前）。
    """

    def __init__(
        self,
        env: EnvLike,
        *,
        api_key: str | None = None,
        max_size: int = DEFAULT_MAX_SIZE,
        open_timeout: float = DEFAULT_OPEN_TIMEOUT,
        limits: ChunkLimits = DEFAULT_LIMITS,
        clock: Clock = time.monotonic,
    ) -> None:
        self.env = env
        self.http = DriverHttp(env.gateway_url, api_key, started_at=env.started_at)
        self.frames = FrameLog()
        self.timeline = Timeline("driver", started_at=env.started_at)
        """连接级时间线：握手 ``connected`` 与"不属于任何已知会话"的事件。"""
        self.ws: GatewayWS | None = None
        self._api_key = api_key
        self._max_size = max_size
        self._open_timeout = open_timeout
        self._limits = limits
        self._clock = clock
        self._sessions: dict[str, Session] = {}

    # ── 生命周期 ──────────────────────────────────────────────

    @classmethod
    async def connect(cls, env: EnvLike, **kwargs: Any) -> Driver:
        """连接网关（WS 握手 + HTTP 客户端），返回可用的 driver。"""
        driver = cls(env, **kwargs)
        await driver.start()
        return driver

    async def start(self) -> None:
        """建立 WS 连接（幂等）。"""
        if self.ws is not None:
            return
        self.ws = await GatewayWS.connect(
            ws_url(self.env.gateway_url),
            api_key=self._api_key,
            max_size=self._max_size,
            open_timeout=self._open_timeout,
            limits=self._limits,
            handler=self._on_event,
            frames=self.frames,
            started_at=self.env.started_at,
            clock=self._clock,
        )

    async def close(self) -> None:
        """关闭连接（幂等，不抛——失败场景的 teardown 也要能安全收尾）。"""
        ws, self.ws = self.ws, None
        if ws is not None:
            await ws.close()
        await self.http.close()

    # ── 属性 ──────────────────────────────────────────────────

    @property
    def websocket(self) -> GatewayWS:
        """WS 连接（未连接时抛 ``DriverError``）。"""
        if self.ws is None:
            raise DriverError("driver is not connected (call Driver.connect first)")
        return self.ws

    @property
    def client_id(self) -> str:
        return self.websocket.client_id

    @property
    def sessions(self) -> list[Session]:
        """已挂载的会话（含 fork 出来的）。"""
        return list(self._sessions.values())

    def get(self, session_id: str) -> Session:
        try:
            return self._sessions[session_id]
        except KeyError as exc:
            raise DriverError(
                f"session {session_id!r} is not attached to this driver "
                f"(attached: {sorted(self._sessions)})"
            ) from exc

    # ── 会话工厂 ──────────────────────────────────────────────

    async def session(
        self,
        *,
        model: str | None = None,
        provider: str | None = None,
        tools: Sequence[str] | None = None,
        yolo: bool | None = None,
        system_prompt: str | None = None,
        append_system_prompt: str | None = None,
        max_turns: int | None = None,
        effort: str | None = None,
        agent: Mapping[str, Any] | None = None,
        template: str | None = None,
        workspace: str | Path | None = None,
        backend: str | None = None,
        subscribe: bool = True,
    ) -> Session:
        """创建并订阅一个新会话。

        便捷参数（``model`` / ``provider`` / ``tools`` / ``yolo`` / ``system_prompt`` /
        ``append_system_prompt`` / ``max_turns`` / ``effort``）合并进 ``agent``
        （``AgentOverride``）下发——同名键以便捷参数为准；``None`` 表示不覆盖。
        """
        overrides: dict[str, Any] = dict(agent or {})
        for key, value in (
            ("model", model),
            ("provider", provider),
            ("tools", list(tools) if tools is not None else None),
            ("yolo", yolo),
            ("system_prompt", system_prompt),
            ("append_system_prompt", append_system_prompt),
            ("max_turns", max_turns),
            ("effort", effort),
        ):
            if value is not None:
                overrides[key] = value
        response = await self.http.create_session(
            template_name=template,
            workspace=str(workspace) if workspace is not None else None,
            agent=overrides or None,
            backend=backend,
        )
        session_id = response.get("session_id")
        if not isinstance(session_id, str):
            raise DriverError(f"create response has no session_id: {response!r}")
        return await self.attach(
            session_id, response=response, workspace=workspace, subscribe=subscribe
        )

    async def resume(self, session_id: str, *, subscribe: bool = True) -> Session:
        """恢复磁盘上的已有会话并订阅它。"""
        response = await self.http.resume_session(session_id)
        return await self.attach(session_id, response=response, subscribe=subscribe)

    async def attach(
        self,
        session_id: str,
        *,
        response: Mapping[str, Any] | None = None,
        workspace: str | Path | None = None,
        subscribe: bool = True,
    ) -> Session:
        """把会话挂进事件路由表（并可选地订阅）——已挂载则原样返回。"""
        existing = self._sessions.get(session_id)
        if existing is not None:
            return existing
        session = Session(self, session_id, response=response, workspace=workspace)
        # 失败报告末行引用现场转储路径（转储本身由 ``probe.dump()`` / fixture
        # teardown 落到同一目录，见 design D8）——断言超时时不必重跑即可取现场。
        session.watch.dump_path = str(self.env.artifacts_path)
        # 先注册路由再订阅：subscribe 会触发 sync_session 重放，事件不能丢。
        self._sessions[session_id] = session
        if subscribe:
            await self.http.subscribe(session_id, self.client_id)
        return session

    # ── 事件路由 ──────────────────────────────────────────────

    def _on_event(
        self,
        session_id: str | None,
        event_type: str,
        data: dict[str, Any],
        delivery: Delivery,
    ) -> None:
        """WS 读任务回调：把完整事件投进对应会话的时间线（未知会话进 driver 时间线）。"""
        target = self._sessions.get(session_id) if session_id else None
        timeline = target.timeline if target is not None else self.timeline
        timeline.append(
            event_type,
            data,
            raw=delivery.text,
            at=delivery.at,
            frames=delivery.frames,
        )


__all__ = [
    "DEFAULT_LIMITS",
    "DEFAULT_TURN_WITHIN",
    "Driver",
    "DriverError",
    "EnvLike",
    "Session",
    "ws_url",
]
