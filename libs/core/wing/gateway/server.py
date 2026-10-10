# wing/gateway/server.py — Gateway 服务器

"""
Gateway 服务器——生命周期管理器 + EventBus 事件路由。

Gateway 不感知 session_id，只感知 client_id：维护 {client_id: ws} 与
{ws: client_id} 两张表，subscribe EventBus 后按 EventTarget 转发到对应 ws，
断连时清理 Gateway 与 EventBus 的路由表。

FastAPI app 创建和路由注册在 app.py 中完成（App Factory 模式）。
"""

from __future__ import annotations

import asyncio
import json
import socket
import sys
from datetime import datetime, timezone
from typing import Any

from fastapi import WebSocket
import uvicorn

from wing.background import BackgroundScheduler
from wing.build_info import get_commit
from wing.common.logger import log, setup_logger
from wing.commands import register_prompt_commands
from wing.config import (
    AuthConfig,
    Config,
    ConfigProblem,
    ProblemKind,
    load_config,
)
from wing.config.boot import BootFailure, BootResult, boot_config
from wing.event import WingEvent, wire_dump
from wing.event_bus import event_bus
from wing.request_context import get_request_context
from wing.runtime import WingRuntime, WriteEffectResult
from wing.system import ReloadResult, ReloadResultItem

from .app import create_app
from .frames import HARD_LIMIT_BYTES, Frame, build_frames
from .remote_tools import RemoteToolManager
from .setup_guard import SetupModeError

#: 默认监听地址（与 ``GatewayConfig.host`` 的声明默认值同源）。
#: 配置不可用时 cli 回落它——两边读同一份文件、同一个默认（总设计 §8.6）。
DEFAULT_HOST = "127.0.0.1"

DEFAULT_PORT = 32523

# 单帧写超时（秒）：向一个客户端的单次发送超过它即判为慢/死消费者并回收。
# 硬编码——对"单帧 ≤8 MiB 的内网投递"极其宽裕（正常是毫秒级），对"永远不读"
# 的客户端足够快。不引入发送队列/背压池：投递模型仍是每事件一次 create_task。
WRITE_TIMEOUT_SECONDS = 60.0

# 回收时关闭连接的上界（秒）：对手正是"不读的客户端"，回收路径自己不能挂住。
CLOSE_TIMEOUT_SECONDS = 5.0


def _check_port_available(host: str, port: int) -> bool:
    """检查端口是否可用，不可用时返回 False。

    使用 connect 而非 bind 来检测——避免 TIME_WAIT 误报。
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        try:
            s.settimeout(1)
            s.connect((host, port))
            return False
        except (OSError, socket.timeout):
            return True


#: setup mode 替身（``_SetupRuntime``）唯一放行的公开入口：保存事务本身。
#: 其余一切公开属性（含继承自 ``WingRuntime`` 的方法）都抛 ``SetupModeError``。
_SETUP_RUNTIME_ALLOWED: frozenset[str] = frozenset(
    {"apply_settings", "post_write_effect"}
)


class _SetupRuntime(WingRuntime):
    """setup mode 的 runtime 替身：**只服务保存事务**（见 :attr:`GatewayServer.runtime`）。

    为什么需要它：真 ``WingRuntime`` 在 setup mode 下构造不出来（``__init__`` 第一个动作
    就是 ``get_config()``），而 ``POST /api/settings/set`` **必须**能用——它是唯一的修复
    路径，且它的实现体是 ``WingRuntime.apply_settings``（唯一写盘路径）。这个替身因此
    不是「第二份事务」，而是**同一个事务 + 不同的第 ⑦ 步效应**
    （:meth:`WingRuntime.post_write_effect`）：正常模式热重载，这里转入正常模式（§8.5）。

    其余一切访问与「runtime 在 setup mode 不可用」同义：守门中间件是业务面的一道闸
    （白名单之外一律 503 ``setup_mode``），替身把**所有**非事务入口（含继承来的方法，
    如 ``list_models`` / ``list_sessions``）都翻成 :class:`SetupModeError`——两道闸
    覆盖同一批路径。
    """

    def __init__(self, server: GatewayServer) -> None:
        # 不调 super().__init__()：它读 config、建 SessionManager / reaper，setup mode 做不到。
        self._server = server

    async def post_write_effect(self) -> WriteEffectResult:
        """⑦：转入正常模式（而不是热重载）——``setup_mode_exited`` 因此为真。"""
        reload_result = self._server._enter_operational()
        return WriteEffectResult(
            reload=reload_result,
            setup_mode_exited=not self._server.in_setup_mode,
        )

    def __getattribute__(self, name: str) -> Any:
        """setup mode 下 **只有保存事务可达**（``__getattr__`` 只拦"基类没有"
        的属性，`list_models` 这类真实方法会绕过去、以 ``ValueError`` 露出）。

        这里用 ``__getattribute__`` 拦**所有**公开属性：白名单只有
        ``apply_settings`` / ``post_write_effect`` 两个入口，其余（含继承来的方法）
        一律 :class:`SetupModeError`。下划线开头的名字（``_server`` / 内省的 dunder）
        放行——它们不是业务入口，拦掉只会让 ``isinstance`` / ``repr`` / 调试工具出怪。
        """
        if not name.startswith("_") and name not in _SETUP_RUNTIME_ALLOWED:
            raise SetupModeError(
                object.__getattribute__(self, "_server").setup_problems
            )
        return object.__getattribute__(self, name)


class GatewayServer:
    """Gateway 服务器——生命周期管理器。

    持有 runtime、client 映射、FastAPI app。
    负责 start/stop 和 EventBus 事件路由。

    **setup mode（04）**：配置不可用时降级启动——``runtime`` 是只服务保存事务的替身，
    守门中间件（``setup_guard.py``）只放行设置端点，其余一律 503 ``setup_mode``；
    保存出合法配置后由 ``_enter_operational()`` **就地**转入正常模式（不重启进程）。
    """

    def __init__(
        self,
        host: str = DEFAULT_HOST,
        port: int = DEFAULT_PORT,
        *,
        boot: BootResult | None = None,
    ) -> None:
        """
        Args:
            boot: 已经做过的启动读取（``cli.py`` 传进来避免二次读盘，且
                ``boot_reason`` 与横幅口径一致）。``None`` ⇒ 自己 ``boot_config()``。
        """
        self.host = host
        self.port = port
        self._boot = boot if boot is not None else boot_config()
        # setup mode 的替身：``runtime`` 因此恒非 Optional——配置不可用不是
        # 「有时没有 runtime」，而是「除修复路径外一律 503」（守门 + 替身共同保证）。
        self._runtime: WingRuntime = _SetupRuntime(self)
        self._in_setup_mode = True
        self._client_to_ws: dict[str, WebSocket] = {}
        self._ws_to_client: dict[WebSocket, str] = {}
        self._remote_tools = RemoteToolManager()
        # 后台周期任务宿主（首个 job：空闲会话逐出）。interval 在**进入正常模式**时
        # 读取一次——config 热重载不改变已注册 job 的间隔；TTL 每个 sweep 都从当前
        # config 读，热重载即时生效。setup mode 下没有 reaper 可 attach，job 也不注册。
        self._background = BackgroundScheduler()
        self._background_requested = False
        self._started_at = datetime.now(timezone.utc)
        if self._boot.ok:
            result = self._enter_operational(config=self._boot.config)
            if not result.ok:
                # AD14：启动路径**必须消费**这个结果——否则「文件合法但运行时装不上」
                # 会静默降级：boot_reason=None、problems 为空、503 说「共 0 条问题」，
                # 而 status.valid=true（预检把用户送进正常启动链再吃一串 503）。
                self._record_startup_failure(result)
        self._app = create_app(self)

    # ============================================================
    # 运行模式
    # ============================================================

    @property
    def runtime(self) -> WingRuntime:
        """当前 runtime（**非 Optional**：配置不可用不是「有时没有 runtime」）。

        正常模式：真 ``WingRuntime``。setup mode：:class:`_SetupRuntime` 替身——
        只有保存事务（``apply_settings``）可达，其余一切访问抛 ``SetupModeError``
        （守门中间件保证那些路径不可达，类型检查也看不到 Optional）。
        """
        return self._runtime

    @runtime.setter
    def runtime(self, value: WingRuntime) -> None:
        """注入接缝：既有测试用 ``server.runtime = mock`` 换掉真实例（行为不变）。

        不翻转模式标志——替身与 :attr:`in_setup_mode` 的一致性由
        :meth:`_enter_operational` 的尾部原子翻转保证。
        """
        self._runtime = value

    @property
    def in_setup_mode(self) -> bool:
        """是否处于 setup mode（04）：只服务设置端点，其余一律 503。"""
        return self._in_setup_mode

    @property
    def setup_problems(self) -> list[ConfigProblem]:
        """启动时配置失败的原因（``boot_config()`` 的产物；setup mode 下即 503 的文案素材）。

        是**启动时的快照**，不每次读盘：「现在文件里还有什么问题」由
        ``GET /api/settings/status`` 现读现报——两者职责不同。
        """
        return list(self._boot.problems)

    @property
    def boot_reason(self) -> BootFailure | None:
        """``boot_config()`` 的结局分类（``ok=True`` 时为 ``None``）。"""
        return self._boot.reason

    def _enter_operational(self, config: Config | None = None) -> ReloadResult:
        """把网关推进正常模式（六步，**幂等**）：① 配置 ② 日志级别 ③ prompt commands
        ④ 建 runtime ⑤ 逐出 job + 后台任务 ⑥ auth 锁死警告。

        任一步失败 ⇒ **停在 setup mode**（``_runtime`` 与 ``_in_setup_mode`` 都不翻转），
        返回 ``ReloadResult(ok=False, items=[…])`` 如实报明细；文件**不回滚**——它是
        合法配置（保存事务已校验过），下次保存或重启再试。

        Args:
            config: 已校验的配置。``None`` ⇒ ``load_config(reload=True)`` 现读磁盘
                （保存事务刚写完盘，此处必然成功）。
        """
        if not self._in_setup_mode:
            return ReloadResult(ok=True, items=[])  # 幂等：已在正常模式

        items: list[ReloadResultItem] = []

        # ① 配置（boot 成功时由调用方给，避免重复读盘）。
        try:
            if config is None:
                config = load_config(reload=True)
        except Exception as e:
            items.append(ReloadResultItem(name="config.yaml", ok=False, detail=str(e)))
            log.error(f"setup mode: cannot enter operational mode — {e}")
            return ReloadResult(ok=False, items=items)
        items.append(ReloadResultItem(name="config.yaml", ok=True))

        # ② 日志级别（config 已进单例；此后所有日志按新级别走）。
        # context= 必须重传：setup_logger 会 handlers.clear() 后重挂，不传就等于
        # 把 #179 的 session / request 关联段抹掉（cli.py 的正常启动路径同样传它）。
        try:
            setup_logger(level=config.log.level, context=get_request_context)
        except Exception as e:
            items.append(ReloadResultItem(name="log level", ok=False, detail=str(e)))
            return ReloadResult(ok=False, items=items)
        items.append(ReloadResultItem(name="log level", ok=True))

        # ③ prompt 命令（配置里的 commands.paths）。
        try:
            register_prompt_commands(config.commands.paths)
        except Exception as e:
            items.append(
                ReloadResultItem(name="prompt commands", ok=False, detail=str(e))
            )
            return ReloadResult(ok=False, items=items)
        items.append(ReloadResultItem(name="prompt commands", ok=True))

        # ④ runtime（hooks 随之加载；providers 池按需懒建）。
        try:
            runtime = WingRuntime()
        except Exception as e:
            items.append(ReloadResultItem(name="runtime", ok=False, detail=str(e)))
            log.error(f"setup mode: cannot enter operational mode — {e}")
            return ReloadResult(ok=False, items=items)
        items.append(ReloadResultItem(name="runtime", ok=True))

        # ⑤ 逐出 job + 后台任务（lifespan 的 startup 已跑过时补启；幂等）。
        try:
            self._install_eviction_job(config, runtime)
            if self._background_requested:
                runtime.reaper.attach()
                self._background.start()
        except Exception as e:
            items.append(
                ReloadResultItem(name="background jobs", ok=False, detail=str(e))
            )
            return ReloadResult(ok=False, items=items)
        items.append(ReloadResultItem(name="background jobs", ok=True))

        # ⑥ auth 锁死警告（读**新**配置——auth_config 在 setup mode 下是安全默认值）。
        detail = self._warn_auth_lockout(config.gateway.auth)
        items.append(ReloadResultItem(name="auth", ok=True, detail=detail))

        # 全部成功：同一个同步块里原子翻转（请求处理之间看不到中间态）。
        self._runtime = runtime
        self._in_setup_mode = False
        log.info(
            "setup mode exited: gateway is operational (config reloaded from disk)"
        )
        return ReloadResult(ok=True, items=items)

    def _record_startup_failure(self, result: ReloadResult) -> None:
        """启动路径转入正常模式失败：**不静默降级**（AD14）。

        做两件事：① ERROR 日志（含逐项明细）；② 把失败写成一条 ``path=None`` 的
        boot 级 problem——于是 ``boot_reason`` 非 None、``setup_problems`` 非空、
        503 detail 不再说「共 0 条问题」，而 ``status.valid`` 因
        ``valid == (not in_setup_mode and 无 problem)`` 一致地为 ``false``。
        """
        detail = "; ".join(
            f"{item.name}: {item.detail or 'failed'}"
            for item in result.items
            if not item.ok
        )
        log.error(f"setup mode: cannot enter operational mode at startup — {detail}")
        self._boot = BootResult(
            ok=False,
            config=None,
            problems=[
                ConfigProblem(
                    path=None,
                    kind=ProblemKind.INVALID_VALUE,
                    message=f"配置合法但运行时装配失败：{detail}",
                    hint="检查运行环境（日志目录写权限 / hooks / store 路径）后重启；"
                    "配置文件本身没有内容问题",
                )
            ],
            reason=BootFailure.INVALID,
            path=self._boot.path,
            endpoint=self._boot.endpoint,
        )

    def _install_eviction_job(self, config: Config, runtime: WingRuntime) -> None:
        """注册空闲会话逐出 job（幂等：重复调用不会留下两个 job）。

        先摘除同名 job 再注册：转入失败后的重试会拿到一个**新的** runtime
        （同一份配置、新的 SessionManager），旧 job 若留着会把逐出器绑在旧
        manager 上——remove + add 是这里的 replace 语义（名字相同，
        ``jobs`` 里始终只有一个）。
        """
        self._background.remove_job("session-eviction")
        self._background.add_job(
            "session-eviction",
            config.sessions.eviction.sweep_interval_seconds,
            runtime.reap_idle_sessions,
        )

    @property
    def uptime(self) -> int:
        """Gateway 运行时长（秒）。"""
        return int((datetime.now(timezone.utc) - self._started_at).total_seconds())

    @property
    def auth_config(self) -> AuthConfig:
        """当前鉴权配置（每次读取最新单例，热重载后立即生效）。

        setup mode：配置读不出来 ⇒ 返回安全默认值（auth 关闭、无 key）。真正的
        来源策略不在这里——守门中间件与 ``AuthMiddleware`` 的 loopback-only 才是
        （§8.4）；这里只保证「任何时序下读 auth 配置都不炸」。
        """
        if self._in_setup_mode:
            return AuthConfig()
        return load_config().gateway.auth

    def _warn_auth_lockout(self, auth: AuthConfig | None = None) -> str | None:
        """检查 auth 配置，空 keys 锁死时发出警告；返回告警文案（未锁死 = ``None``）。

        ``auth`` 给定时用它——``_enter_operational()`` 的 ⑥ 要读**新**配置，而
        ``auth_config`` 在 setup mode 下返回的是安全默认值。
        """
        auth = auth if auth is not None else self.auth_config
        if auth.enabled and not auth.keys:
            message = (
                "gateway.auth.enabled=true but keys list is empty — "
                "ALL requests (including /api/system/reload) will be "
                "rejected with 401. Edit config.yaml and restart to fix."
            )
            log.warning(message)
            return message
        return None

    @property
    def clients(self) -> dict[str, WebSocket]:
        """client_id → WebSocket 映射，供 routes 访问。"""
        return self._client_to_ws

    @property
    def ws_to_clients(self) -> dict[WebSocket, str]:
        """WebSocket → client_id 映射，供 routes 访问。"""
        return self._ws_to_client

    @property
    def remote_tools(self) -> RemoteToolManager:
        """远程工具管理器，供 routes（ws / tools）访问。"""
        return self._remote_tools

    def start_background(self) -> None:
        """启动后台周期任务（gateway lifespan startup 调用）。

        setup mode：此刻没有 reaper 可 attach —— 记下「lifespan 已跑过」的意图，
        转入正常模式时在 ``_enter_operational()`` 的 ⑤ 补上（幂等）。
        """
        self._background_requested = True
        if self._in_setup_mode:
            return
        self.runtime.reaper.attach()
        self._background.start()

    async def stop_background(self) -> None:
        """停止后台周期任务（gateway lifespan shutdown 调用）；setup mode 下无 reaper 可 detach。"""
        await self._background.stop()
        if not self._in_setup_mode:
            self.runtime.reaper.detach()

    def start(self) -> None:
        """启动服务器（阻塞）。"""
        if not _check_port_available(self.host, self.port):
            print(f"❌ 端口 {self.port} 已被占用，请指定其他端口或释放占用。")
            sys.exit(1)

        self._warn_auth_lockout()

        event_bus.subscribe(self._on_event)

        print(
            f"🚀 Gateway 启动于 {self.host}:{self.port} (commit {get_commit() or 'unknown'})"
        )
        uvicorn.run(
            self._app,
            host=self.host,
            port=self.port,
            log_config=None,
        )

    def _on_event(self, event: WingEvent) -> None:
        """EventBus subscriber callback：根据 EventTarget 路由事件到 ws。

        同步回调，内部 asyncio.create_task 调度异步 send。
        tool_runtime（纯工具执行远端）不接收事件——global 广播与 client
        定向都跳过它，闭合"不参与事件订阅"的边界。

        帧内容在 create_task 之前 eager 序列化定型（统一 wire 出口
        `wire_dump`）：保证送达顺序等于发射顺序，且不依赖任何延迟序列化
        的时序（事件对象广播后不再被改写）。每个事件只序列化一次。
        """
        target = event.target
        if target is None:
            return

        payload = json.dumps(wire_dump(event))
        # 切分也在 eager 阶段完成（与序列化同理）：帧内容定型后再交给 task，
        # 不依赖任何延迟序列化的时序。小载荷原样单帧，零额外开销。
        frames = build_frames(payload, event.type)

        if target.scope == "global":
            for cid, ws in list(self._client_to_ws.items()):
                if not self._receives_events(cid):
                    continue
                self._schedule_send(ws, frames, event.type)

        elif target.scope == "client":
            for cid in target.client_ids:
                ws = self._client_to_ws.get(cid)
                if ws is not None and self._receives_events(cid):
                    self._schedule_send(ws, frames, event.type)

    def _schedule_send(self, ws: WebSocket, frames: list[Frame], of_type: str) -> None:
        """调度一个事件的投递（每事件一次 task，同一事件的帧在该 task 内按序发出）。"""
        try:
            asyncio.get_running_loop().create_task(
                self._send_frames(ws, frames, of_type)
            )
        except RuntimeError:
            pass  # 无运行中的事件循环（进程收尾）——与既有行为一致

    def _receives_events(self, client_id: str) -> bool:
        """client 是否接收事件。tool_runtime（attached 且 receives_events=False）
        被跳过；纯前端（未 attach）默认接收。"""
        return not (
            self._remote_tools.is_attached(client_id)
            and not self._remote_tools.receives_events(client_id)
        )

    async def drop_client(self, ws: WebSocket, *, reason: str) -> str | None:
        """回收一个客户端连接——**唯一的清理入口**，幂等。

        两条路径共用：`handle_ws` 的正常断连收尾（客户端已经走了，不需要
        再关连接）与慢消费者回收（写超时/写失败，调用方随后主动关连接）。
        两份平行实现必然漂移——`fail_client` 的 KV cache 保护、路由表清理
        这些不变量只能有一个家。

        幂等靠 ``ws_to_clients.pop`` 的返回值：只有"第一次拿到回收权"的
        调用者产生副作用（关连接、fail_client、日志）。并发的多个
        `_send_text` 同时失败、或发送失败紧接 `handle_ws` 的 finally 时，
        只有一个赢家——这也是"每个死客户端最多一条回收日志"的结构性保证。

        返回 client_id（首次回收）；已被回收 / 从未登记返回 None。
        """
        client_id = self._ws_to_client.pop(ws, None)
        if client_id is None:
            return None
        self._client_to_ws.pop(client_id, None)
        event_bus.route_detach_client(client_id)
        if self._remote_tools.is_attached(client_id):
            # 在途调用立即失败 + 注销远程工具（敏锐检测断连）
            self._remote_tools.fail_client(client_id, reason)
        log.info(f"Client disconnected: {client_id} ({reason})")
        return client_id

    async def _recycle_client(self, ws: WebSocket, reason: str) -> None:
        """回收慢/死消费者：先注销（投递立即停止），再尽力关闭连接。"""
        client_id = await self.drop_client(ws, reason=reason)
        if client_id is None:
            return
        try:
            await asyncio.wait_for(
                ws.close(code=1013, reason="slow consumer"),
                timeout=CLOSE_TIMEOUT_SECONDS,
            )
        except Exception as e:
            # 关闭失败无需补救：路由与投递列表已经清干净，连接由 ASGI 层收尸。
            # 回收路径绝不能因为对端不读而挂住。
            log.debug(f"Failed to close recycled client {client_id}: {e}")

    async def _send_frames(
        self, ws: WebSocket, frames: list[Frame], of_type: str
    ) -> None:
        """一个事件的投递单元：同一事件的帧在**同一个 task 内**按序发出。

        硬上限（16 MiB，= 客户端单帧上限）是最终契约：任何仍超限的帧在这里
        被拦截丢弃 + 一行 WARN（应用层照常 emit，丢的只是这一帧；不新增计数
        指标）。切分正常时每帧 ≤ 软上限，本分支是安全网而非控制流。

        客户端被回收后（写超时 / 写失败）立即停止剩余帧——不产生二次回收、
        不产生重复失败日志。
        """
        client_id = self._ws_to_client.get(ws, "unknown")
        for frame in frames:
            if frame.size > HARD_LIMIT_BYTES:
                log.warning(
                    f"Dropping oversized frame for client {client_id}: "
                    f"type={of_type} size={frame.size} > {HARD_LIMIT_BYTES}"
                )
                continue
            if not await self._send_text(ws, frame.text):
                return

    async def _send_text(self, ws: WebSocket, data: str) -> bool:
        """异步发送文本到 ws——有界等待，超时/失败即回收该客户端。

        投递模型未变（每事件一次 create_task，帧逐次发送）。这里的上界是
        "无限期"与"有限期"的分界：写超时（或写失败）后该 client 从路由表
        消失，后续事件不再投递（`_on_event` 的投递列表就是路由表），因此不会
        再有失败投递与日志洪泛。

        返回该客户端是否**仍然有效**：False 表示已被回收，调用方（分片发送
        循环）不应继续尝试投递剩余帧。

        边界说明：uvicorn 的 WS 实现把未写完的数据放进用户态缓冲，
        `send_text` 往往立即返回——此时真正触发回收的是异常分支（连接已死 /
        ASGI 已关闭）。两条分支走同一条回收路径，行为一致。
        """
        try:
            await asyncio.wait_for(ws.send_text(data), timeout=WRITE_TIMEOUT_SECONDS)
            return True
        except TimeoutError:
            log.error(
                f"Dropping slow client: send blocked for >{WRITE_TIMEOUT_SECONDS:.0f}s"
            )
            await self._recycle_client(
                ws, f"write timeout after {WRITE_TIMEOUT_SECONDS:.0f}s"
            )
            return False
        except Exception as e:
            log.error(f"Failed to send to client: {e}")
            await self._recycle_client(ws, f"send failed: {e}")
            return False
