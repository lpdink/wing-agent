"""``Probe`` —— 场景门面（tasks 4.7 / 6.1）。

``Probe`` 把前面几轮的积木（``ProbeEnv`` 环境自举、``Driver`` 会话驱动、
``Watcher`` 事件断言、``HistoryView`` 落盘视图、``FileAssertions`` 文件断言、
``RequestLog`` 请求留档）收成一个对象，供 fixture 与场景直接用：

    probe = await Probe.start(tmp_path / "probe")
    probe.register("probe/basic", Turn.of(text="done"))
    session = await probe.session(model="probe/basic", yolo=True)
    await session.chat("hi")
    probe.files.assert_content("out.txt", contains="done")
    probe.history(session).assert_chain_invariants()

设计取舍（design D1/D5/D8）：

- **Probe 与 ProbeEnv 分离**：``Probe`` 只做"给人用"的门面，环境生命周期仍是
  ``ProbeEnv`` 的职责——将来把 env 提升到 session 级共享时只需改 fixture 作用域；
- **不变量不可静默关闭**：``without_invariants(reason=...)`` 必须给理由，理由
  写进现场转储与失败报告（spec「内置不变量自动运行」）；
- **``dump()`` 是现场快照**：时间线全帧 + 原始帧 + HTTP 留档 + 假 Provider 请求
  留档 + 各 session 落盘拷贝 + 网关日志，写进 ``<env.root>/artifacts/``，
  失败报告引用该路径（spec「失败报告与现场转储」）。
"""

from __future__ import annotations

import json
import logging
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any

from wing_probe.driver.http import HttpCall
from wing_probe.driver.session import Driver, Session
from wing_probe.env import ProbeEnv, log_section, read_log_tail
from wing_probe.files import FileAssertions
from wing_probe.history.invariants import (
    HistoryAssertionError,
    assert_chain_invariants,
    assert_no_transient_records,
    assert_tool_pairing,
)
from wing_probe.history.view import HistoryView, read_metadata
from wing_probe.driver.ws import GatewayWS
from wing_probe.provider.context import ContextView
from wing_probe.provider.request_log import LoggedRequest, RequestLog
from wing_probe.provider.script import Script, Turn
from wing_probe.provider.server import FakeProvider
from wing_probe.watch.timeline import Frame, Timeline

_log = logging.getLogger("wing_probe.probe")

#: 默认 workspace 目录名（``<root>/workspace``）——场景不指定 workspace 时用它。
DEFAULT_WORKSPACE_DIRNAME = "workspace"

#: 现场转储的固定文件名。
DUMP_SUMMARY = "dump.txt"
DUMP_TIMELINE = "timeline.jsonl"
DUMP_FRAMES = "frames.jsonl"
DUMP_HTTP = "http.jsonl"
DUMP_REQUESTS = "requests.json"
DUMP_LOG_TAIL = "gateway.log.tail"
DUMP_LOG = "gateway.log"
DUMP_SESSIONS = "sessions"

#: 网关日志全文拷贝的上限（超过则只留尾部，避免 artifacts 里塞进巨大文件）。
DUMP_LOG_LIMIT = 2 * 1024 * 1024

#: 内置不变量检查：名字（进失败报告，spec 要求"具体不变量名"）+ 断言函数。
INVARIANT_CHECKS: tuple[tuple[str, Callable[[HistoryView], None]], ...] = (
    ("chain_topology", assert_chain_invariants),
    ("tool_pairing", assert_tool_pairing),
    ("no_transient_records", assert_no_transient_records),
)


class ProbeError(RuntimeError):
    """门面用法错误（无理由的逃生舱、未注册的模型…）。"""


class ProbeInvariantError(AssertionError):
    """内置不变量在场景结束时失败（fixture teardown 抛出 → 场景失败）。

    报告标注来源（``built-in invariants``）并逐条给出 session id + 不变量名 +
    底层断言报告——与场景自身断言并列，而不是被吞掉。
    """

    def __init__(self, report: str) -> None:
        self.report = report
        super().__init__(report)


def run_invariants(views: Sequence[HistoryView]) -> list[str]:
    """对一个 session 的全部视图运行三条内置不变量，返回问题清单（不抛）。

    每条问题形如 ``[chain_topology] session <id>: <底层报告>``——来源、位置与
    原因都在一行里可读。
    """
    checks = INVARIANT_CHECKS
    problems: list[str] = []
    for view in views:
        for name, check in checks:
            try:
                check(view)
            except HistoryAssertionError as exc:
                problems.append(
                    f"[{name}] session {view.session_id} ({view.path}):\n{exc}"
                )
    return problems


class Probe:
    """一个场景的全部断言面（env + driver + 视图 + 现场转储）。

    用法（见 ``scenarios/conftest.py`` 的 ``probe`` fixture）：

        probe = await Probe.start(tmp_path / "probe")
        probe.register("probe/x", Turn.of(text="hi"))
        session = await probe.session(model="probe/x")
        await session.chat("hello")
        probe.files.assert_content("out.txt", contains="ok")
    """

    def __init__(
        self,
        env: ProbeEnv,
        *,
        driver: Driver | None = None,
        workspace: str | Path | None = None,
    ) -> None:
        self.env = env
        self.driver = driver
        self.workspace = (
            Path(workspace).expanduser().resolve()
            if workspace is not None
            else (env.root / DEFAULT_WORKSPACE_DIRNAME)
        )
        """默认 workspace（``probe.session()`` 不带 workspace 时的会话工作目录）。"""
        self.invariants_reason: str | None = None
        """``without_invariants(reason)`` 的理由（None = 不变量正常运行）。"""
        self._last_dump: Path | None = None
        self._checked_session_ids: list[str] = []

    # ── 生命周期 ──────────────────────────────────────────────

    @classmethod
    async def start(
        cls,
        root: str | Path,
        *,
        workspace: str | Path | None = None,
        connect: bool = True,
        **env_kwargs: Any,
    ) -> Probe:
        """自举环境（+ 默认 workspace 目录）并连接 driver 与会话断言面。"""
        env = await ProbeEnv.start(root, **env_kwargs)
        probe = cls(env, workspace=workspace)
        probe.workspace.mkdir(parents=True, exist_ok=True)
        try:
            if connect:
                probe.driver = await Driver.connect(env)
        except BaseException:
            await env.stop()
            raise
        return probe

    async def stop(self) -> None:
        """关闭 driver 与网关子进程（幂等、不抛——失败场景的 teardown 也要安全）。"""
        driver, self.driver = self.driver, None
        if driver is not None:
            await driver.close()
        await self.env.stop()

    async def __aenter__(self) -> Probe:
        return self

    async def __aexit__(self, *_: Any) -> None:
        await self.stop()

    # ── 环境 / 假 Provider ────────────────────────────────────

    @property
    def provider(self) -> FakeProvider:
        """进程内假 Provider（剧本注册 / 请求留档）。"""
        return self.env.provider

    @property
    def requests(self) -> RequestLog:
        """假 Provider 的请求留档表（上下文断言的唯一来源）。"""
        return self.env.provider.requests

    @property
    def artifacts_path(self) -> Path:
        """现场转储目录（``<env.root>/artifacts``）。"""
        return self.env.artifacts_path

    def register(self, model: str, script: Script | Turn, *turns: Turn) -> Script:
        """注册剧本：``register(model, Script(...))`` 或 ``register(model, turn_a, turn_b)``。"""
        resolved = script if isinstance(script, Script) else Script(script, *turns)
        self.env.register(model, resolved)
        return resolved

    def request(self, model: str | None = None, index: int = 0) -> LoggedRequest:
        """取一次已留档的 LLM 请求（按 model 与序号；越界抛 ``IndexError``）。"""
        return self.requests.get(model, index)

    def context(self, model: str | None = None, index: int = 0) -> ContextView:
        """取一次请求的 ``ContextView``（上下文断言的入口）。"""
        return self.request(model, index).context()

    def requests_for(self, model: str) -> list[LoggedRequest]:
        """某 model 的全部请求（按到达顺序）。"""
        return self.requests.by_model(model)

    def last_request(self, model: str | None = None) -> LoggedRequest:
        """最近一次请求（可按 model 过滤）。"""
        return self.requests.last(model)

    # ── 会话 ──────────────────────────────────────────────────

    @property
    def websocket(self) -> GatewayWS:
        """WS 连接（driver 未连接时抛 ``DriverError``）。"""
        return self.driver_required.websocket

    @property
    def driver_required(self) -> Driver:
        if self.driver is None:
            raise ProbeError("probe is not connected (Probe.start(connect=False)?)")
        return self.driver

    @property
    def sessions(self) -> list[Session]:
        """已挂载的全部会话（含 fork 出来的子会话）。"""
        return self.driver_required.sessions

    async def session(
        self,
        *,
        model: str | None = None,
        tools: Sequence[str] | None = None,
        yolo: bool | None = None,
        workspace: str | Path | None = None,
        **kwargs: Any,
    ) -> Session:
        """创建一个会话并订阅其事件流（``workspace`` 缺省 = ``probe.workspace``）。"""
        target = self.workspace if workspace is None else Path(workspace)
        target.mkdir(parents=True, exist_ok=True)
        return await self.driver_required.session(
            model=model, tools=tools, yolo=yolo, workspace=target, **kwargs
        )

    async def resume(self, session_id: str) -> Session:
        """恢复磁盘上的会话并订阅（留给"重启后仍在"的断言）。"""
        return await self.driver_required.resume(session_id)

    # ── 断言面 ────────────────────────────────────────────────

    @property
    def files(self) -> FileAssertions:
        """默认 workspace 的文件断言器。"""
        return FileAssertions(self.workspace)

    def files_of(self, session: Session | str) -> FileAssertions:
        """某个会话 workspace 的文件断言器（``session`` 可以是句柄或 session id）。

        句柄优先用其 ``workspace``；只给 session id（或句柄的 workspace 未知，如
        resume 出来的会话）时回读 ``metadata.json.workspace``——否则会静默指到
        默认 workspace 上，让文件断言变成假红/假绿。
        """
        if isinstance(session, Session):
            session_id = session.session_id
            root = session.workspace or self._session_workspace(session_id)
        else:
            root = self._session_workspace(session)
        return FileAssertions(root or self.workspace)

    def _session_workspace(self, session_id: str) -> Path | None:
        """从落盘 metadata 读会话 workspace（缺失 / 空值返回 None）。"""
        metadata = read_metadata(self.env.session_dir(session_id))
        workspace = (metadata or {}).get("workspace")
        return Path(str(workspace)).expanduser() if workspace else None

    def history(self, session: Session | str) -> HistoryView:
        """会话的 ``history.jsonl`` 视图（``session`` 可以是句柄或 session id）。"""
        session_id = session.session_id if isinstance(session, Session) else session
        return HistoryView(self.env.session_dir(session_id))

    # ── HTTP 留档 ─────────────────────────────────────────────

    @property
    def http_calls(self) -> list[HttpCall]:
        """driver 的全部 HTTP 调用留档（断言"网关回了什么"）。"""
        return self.driver_required.http.calls

    @property
    def frames(self) -> list[Frame]:
        """原始 WS 入站帧（含 ``_chunk`` 信封帧；未连接时为空）。"""
        return [] if self.driver is None else self.driver.frames.frames

    def last_http_call(self, *, path: str | None = None) -> HttpCall | None:
        """最近一次 HTTP 调用（可按 path 过滤）。"""
        return self.driver_required.http.last_call(path=path)

    # ── 不变量 ────────────────────────────────────────────────

    @property
    def invariants_enabled(self) -> bool:
        """内置不变量是否仍启用（``without_invariants`` 之后为 False）。"""
        return self.invariants_reason is None

    def without_invariants(self, reason: str) -> None:
        """逃生舱：关闭 teardown 的内置不变量检查——**必须**给出理由。

        理由字符串会写进现场转储（``dump.txt``）与失败报告；空理由直接报错
        （spec「逃生舱必须说明理由」）。

        Raises:
            ProbeError: ``reason`` 为空 / 全空白 / 非字符串。
        """
        if not isinstance(reason, str) or not reason.strip():
            raise ProbeError(
                "without_invariants() requires a non-empty reason string "
                "(the reason is written into the artifacts dump and failure report)"
            )
        self.invariants_reason = reason.strip()

    def check_invariants(self, views: Sequence[HistoryView] | None = None) -> list[str]:
        """对全部已挂载 session 运行内置不变量，返回问题清单（不抛）。

        Args:
            views: 显式指定要检查的视图（默认 = driver 已挂载的全部 session；
                未连接 driver 时为空清单）。

        被检查的 session id 会记下来，供 :meth:`invariant_report` 在 driver 已关闭
        （teardown 之后）时仍能渲染出可读报告。
        """
        resolved = (
            list(views)
            if views is not None
            else [self.history(session) for session in self._attached_sessions()]
        )
        self._checked_session_ids = [view.session_id for view in resolved]
        return run_invariants(resolved)

    def invariant_report(self, problems: Sequence[str]) -> str:
        """把不变量问题渲染成场景失败报告（含来源标注 / session id / 理由）。

        不依赖 driver 是否已关闭（session 清单取自 :meth:`check_invariants`）。
        """
        session_ids = self._checked_session_ids or [
            session.session_id for session in self._attached_sessions()
        ]
        lines = [
            "--- built-in invariants (source: wing_probe fixture teardown) ---",
            f"{len(problems)} problem(s) across {len(session_ids)} session(s) "
            f"created by this scenario: {session_ids}",
        ]
        lines.extend(problems)
        if self.invariants_reason is not None:
            lines.append(
                f"NOTE: probe.without_invariants(reason={self.invariants_reason!r}) "
                "was called for this scenario"
            )
        if self._last_dump is not None:
            lines.append(f"artifacts: {self._last_dump}")
        return "\n".join(lines)

    def _attached_sessions(self) -> list[Session]:
        """driver 已挂载的会话（未连接 driver 时为空清单——dump / teardown 用）。"""
        return [] if self.driver is None else self.driver.sessions

    # ── 现场转储 ──────────────────────────────────────────────

    async def dump(self, path: str | Path | None = None) -> Path:
        """把现场写入 ``<env.root>/artifacts/``（或给定目录），返回该目录。

        内容（spec「失败报告与现场转储」）：

        - ``timeline.jsonl``：全部时间线（含 driver 级）的每个事件——type / data /
          相对时间 / 原始载荷（"全帧"的可读投影）；
        - ``frames.jsonl``：原始 WS 入站帧（含 ``_chunk`` 信封帧，未重组）；
        - ``http.jsonl``：driver 的 HTTP 调用留档（method / path / body / status /
          response）；
        - ``requests.json``：假 Provider 收到的原始请求体（LLM 请求的事实来源）；
        - ``sessions/<id>/history.jsonl`` / ``metadata.json``：落盘拷贝；
        - ``gateway.log`` / ``gateway.log.tail``：网关日志（全文 / 尾部）；
        - ``dump.txt``：环境、会话清单、逃生舱理由与文件清单。
        """
        target = (
            Path(path).expanduser().resolve()
            if path is not None
            else (self.artifacts_path)
        )
        target.mkdir(parents=True, exist_ok=True)
        written: list[str] = []

        written += self._dump_timelines(target)
        written += self._dump_http(target)
        written += self._dump_requests(target)
        written += self._dump_sessions(target)
        written += self._dump_log(target)
        self._dump_summary(target, written)
        self._last_dump = target
        return target

    def _write(self, path: Path, text: str) -> None:
        path.write_text(text, encoding="utf-8")

    def _json_line(self, payload: Mapping[str, Any]) -> str:
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    def _timelines(self) -> list[tuple[str, Timeline]]:
        timelines: list[tuple[str, Timeline]] = []
        if self.driver is not None:
            timelines.append(("driver", self.driver.timeline))
            for session in self.driver.sessions:
                timelines.append((session.session_id, session.timeline))
        return timelines

    def _dump_timelines(self, target: Path) -> list[str]:
        names = [DUMP_TIMELINE, DUMP_FRAMES]
        lines: list[str] = []
        for name, timeline in self._timelines():
            for event in timeline.all():
                record = event.as_dict()
                record["session"] = name
                lines.append(self._json_line(record))
        self._write(target / DUMP_TIMELINE, "".join(f"{line}\n" for line in lines))

        frames: list[str] = []
        for frame in self.frames:
            frames.append(
                self._json_line(
                    {
                        "at": round(frame.at, 6),
                        "chunk": frame.chunk,
                        "text": frame.text,
                    }
                )
            )
        self._write(target / DUMP_FRAMES, "".join(f"{line}\n" for line in frames))
        return names

    def _dump_http(self, target: Path) -> list[str]:
        calls = [] if self.driver is None else self.driver.http.calls
        lines = [
            self._json_line(
                {
                    "method": call.method,
                    "path": call.path,
                    "status": call.status,
                    "at": round(call.at, 6),
                    "duration_ms": round(call.duration_ms, 3),
                    "request": call.body,
                    "response": call.response,
                    "text": call.text if call.response is None else "",
                    "detail": call.detail,
                }
            )
            for call in calls
        ]
        self._write(target / DUMP_HTTP, "".join(f"{line}\n" for line in lines))
        return [DUMP_HTTP]

    def _dump_requests(self, target: Path) -> list[str]:
        payload = [
            {
                "index": entry.index,
                "model_index": entry.model_index,
                "model": entry.model,
                "stream": entry.stream,
                "body": entry.body,
            }
            for entry in self.requests.all()
        ]
        self._write(
            target / DUMP_REQUESTS, json.dumps(payload, ensure_ascii=False, indent=2)
        )
        return [DUMP_REQUESTS]

    def _session_dirs(self) -> list[tuple[str, Path]]:
        """现场里的 session 目录：driver 已挂载的 + 磁盘上存在的（合并去重）。

        后者是"现场真相"的兜底：即使某个 session 没被 driver 挂载（或 driver
        已经断开），转储与摘要也要把它带上。
        """
        found: dict[str, Path] = {
            session.session_id: session.session_dir
            for session in self._attached_sessions()
        }
        sessions_root = self.env.sessions_path
        if sessions_root.is_dir():
            for path in sorted(sessions_root.iterdir()):
                if path.is_dir():
                    found.setdefault(path.name, path)
        return sorted(found.items())

    def _dump_sessions(self, target: Path) -> list[str]:
        written: list[str] = []
        root = target / DUMP_SESSIONS
        root.mkdir(parents=True, exist_ok=True)
        for session_id, session_dir in self._session_dirs():
            dest = root / session_id
            dest.mkdir(parents=True, exist_ok=True)
            for name in ("history.jsonl", "metadata.json", "aux.json"):
                source = session_dir / name
                if source.is_file():
                    (dest / name).write_bytes(source.read_bytes())
                    written.append(f"{DUMP_SESSIONS}/{session_id}/{name}")
        return written

    def _dump_log(self, target: Path) -> list[str]:
        written: list[str] = []
        log_path = self.env.log_path
        self._write(
            target / DUMP_LOG_TAIL,
            log_section(log_path, title="gateway.log tail") + "\n",
        )
        written.append(DUMP_LOG_TAIL)
        try:
            size = log_path.stat().st_size
            if size <= DUMP_LOG_LIMIT:
                (target / DUMP_LOG).write_bytes(log_path.read_bytes())
                written.append(DUMP_LOG)
            else:
                self._write(
                    target / DUMP_LOG,
                    f"(log is {size} byte(s) > {DUMP_LOG_LIMIT}; tail only)\n"
                    + read_log_tail(log_path, max_lines=1000, max_chars=DUMP_LOG_LIMIT),
                )
                written.append(DUMP_LOG)
        except OSError as exc:
            _log.debug("cannot copy gateway log: %s", exc)
        return written

    def _dump_summary(self, target: Path, written: Sequence[str]) -> None:
        sessions = self._session_dirs()
        http_calls = [] if self.driver is None else self.driver.http.calls
        lines = [
            "--- wing-probe artifacts ---",
            f"root: {self.env.root}",
            f"workspace: {self.workspace}",
            f"gateway: {self.env.gateway_url} (bin {self.env.gateway_bin}, "
            f"port {self.env.port})",
            f"fake provider: {self.env.provider_url}",
            f"provider requests: {self.requests.summary()}",
            f"http calls: {len(http_calls)}",
            f"sessions ({len(sessions)}):",
        ]
        for session_id, session_dir in sessions:
            view = HistoryView(session_dir)
            lines.append(
                f"  {session_id}: {len(view.records)} record(s), "
                f"tip={view.tip_uuid}, dir={session_dir}"
            )
        if self.invariants_reason is not None:
            lines.append(
                "built-in invariants DISABLED via probe.without_invariants("
                f"reason={self.invariants_reason!r})"
            )
        if self._last_dump is not None:
            lines.append(f"previous dump: {self._last_dump}")
        lines.append("files:")
        lines.extend(f"  {name}" for name in written)
        self._write(target / DUMP_SUMMARY, "\n".join(lines) + "\n")


__all__ = [
    "DEFAULT_WORKSPACE_DIRNAME",
    "DUMP_FRAMES",
    "DUMP_HTTP",
    "DUMP_LOG",
    "DUMP_LOG_TAIL",
    "DUMP_REQUESTS",
    "DUMP_SESSIONS",
    "DUMP_SUMMARY",
    "DUMP_TIMELINE",
    "INVARIANT_CHECKS",
    "Probe",
    "ProbeError",
    "ProbeInvariantError",
    "run_invariants",
]
