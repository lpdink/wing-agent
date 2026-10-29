#!/usr/bin/env python3
"""一次容量测量的**一档**：进程拓扑、会话建立、采样窗口、样本回收。

拓扑（为什么这样分层、如何避免测出假的退化 → ``README.md``「进程拓扑」）：

    coordinator（本模块，run.py 驱动）
    ├── 假 Provider：同进程内 aiohttp（``wing_probe.provider.FakeProvider``）
    ├── 网关：独立子进程（``wing_probe.env.ProbeEnv``，fresh WING_HOME + OS 分配端口）
    └── driver 分片 ×K：独立子进程（``driver_shard.py``）

一档的生命周期：起环境 → 注册剧本 → coordinator 经 HTTP 建全部会话 → spawn 分片并
等全部就绪 → 下发窗口（``window_start/end`` 是绝对 ``time.monotonic()``）→ 窗口内
采样 → 收分片样本 → 拆环境（异常路径同样收干净）。建会话由 coordinator 统一做：
「建 N 个会话要多久」本身是一个被测数字，且分片不重复写。

失败时**保留已经拿到的证据**：采样挂了也照收分片样本（它们已在窗口末落盘），
建好的会话数与网关身份照常写进 `run.json`，只有真正没测到的字段才是 ``null``。
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import sys
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, Any

import httpx
from wing_probe.env import ProbeEnv, read_log_tail, resolve_gateway_bin

import metrics
import proc
import shapes
from metrics import TurnSample

#: 采样间隔（秒）：RSS / fd 的峰值分辨率。CPU 不走周期采样（见 ``proc``）。
SAMPLE_INTERVAL = 1.0

#: 分片就绪 / 退出的等待上限（秒）。
SHARD_READY_TIMEOUT = 180.0
SHARD_EXIT_TIMEOUT = 60.0

#: 建会话的并发上限（低于 httpx 默认连接池，避免把「建会话」自己变成瓶颈）。
CREATE_CONCURRENCY = 32

#: 网关健康检查上限（秒）：本档要建 N 个会话，冷启动慢机器也够。
GATEWAY_HEALTH_TIMEOUT = 120.0

#: 会话工具集：固定且最小——工具 schema 的体积会摊进每轮请求，属被测变量之外的噪声。
TOOLS: tuple[str, ...] = ("Read",)

SYSTEM_PROMPT = "You are wing's capacity probe agent. Answer briefly."

WORKSPACE_DIRNAME = "workspace"
SHARD_SCRIPT = Path(__file__).with_name("driver_shard.py")

#: ``run.json`` 的指标键（顺序即报告列序）。取不到的一律 ``None`` → JSON ``null``。
METRIC_KEYS: tuple[str, ...] = (
    "gateway.cpu_cores",
    "gateway.rss_mb_peak",
    "gateway.rss_mb_end",
    "gateway.rss_mb_slope_per_session",
    "gateway.fds_peak",
    "delivery.p50_ms",
    "delivery.p99_ms",
    "delivery.p999_ms",
    "turn.p50_ms",
    "turn.p99_ms",
    "tokens_per_second_actual",
    "errors",
    "slow_client_recycles",
)

#: ``run.json`` 的样本记账键（成功档与失败档键集相同，缺数据是 ``None``）。
SAMPLE_KEYS: tuple[str, ...] = (
    "count",
    "dropped",
    "dropped_reasons",
    "text_frames",
    "turns",
    "delivery_available",
)

DELIVERY_UNAVAILABLE = (
    "delivery.* is null: frame_emits not available (the fake provider does not "
    "record per-frame emit times)"
)

#: 分片自报的 CPU 窗口与 coordinator 的采样窗口允许的相对偏差（超过即记 note）。
CPU_WINDOW_TOLERANCE = 0.1


class OrchestratorError(RuntimeError):
    """一档无法继续（网关起不来 / 分片崩溃 / 会话建不全）——本档记失败并继续。"""


@dataclass(frozen=True)
class RunConfig:
    """一次运行的参数（CLI 冻结后的完整输入）。"""

    scenario: str
    levels: tuple[int, ...]
    rate: float
    dwell: float
    warmup: float
    shards: int
    subscribers: str
    history_tokens: int
    out: Path

    @property
    def span_s(self) -> float:
        """每档的驱动时长（warmup + dwell）。"""
        return self.warmup + self.dwell

    def shards_for(self, sessions: int) -> int:
        """本档实际分片数（分片多于会话是纯浪费）。"""
        return max(1, min(self.shards, sessions))


@dataclass
class _ShardHandle:
    """一个分片子进程（配置 / 样本 / 日志三份文件 + 管道握手）。"""

    index: int
    process: asyncio.subprocess.Process
    config_path: Path
    out_path: Path
    log_path: Path
    log_handle: IO[bytes]

    async def stop(self) -> None:
        """收掉分片（幂等；正常退出时只收尸）。"""
        if self.process.returncode is None:
            try:
                self.process.kill()
            except ProcessLookupError:  # pragma: no cover - 竞态
                pass
        await asyncio.gather(self.process.wait(), return_exceptions=True)
        try:
            self.log_handle.close()
        except OSError:  # pragma: no cover - 已关闭
            pass


@dataclass
class _Sampling:
    """窗口内的采样结果（``_sample_window`` 原地填充，失败也保留已采部分）。"""

    window_start: float = 0.0
    window_end: float = 0.0
    wall_s: float = 0.0
    gateway_pid: int | None = None
    gateway_cpu_seconds: float | None = None
    provider_cpu_seconds: float | None = None
    rss_mb: list[float] = field(default_factory=list)
    fds: list[int] = field(default_factory=list)
    loadavg: list[float] | None = None


# ── 一档 ──────────────────────────────────────────────────────


def level_skeleton(sessions: int) -> dict[str, Any]:
    """一档记录的空壳（成功与失败共用同一份键集，缺数据是 ``None``）。"""
    return {
        "sessions": sessions,
        "wall_s": None,
        "metrics": dict.fromkeys(METRIC_KEYS),
        "rig": {
            "provider_cpu_cores": None,
            "shard_cpu_cores": [],
            "host_loadavg": None,
        },
        "samples": dict.fromkeys(SAMPLE_KEYS),
        "session_create": {"ok": None, "failed": None, "total_ms": None},
        "gateway": {"pid": None, "url": None, "version": None, "commit": None},
        "shape": {"rate": None, "shards": None, "subscribers": None},
    }


async def run_level(cfg: RunConfig, sessions: int, level_dir: Path) -> dict[str, Any]:
    """跑一档并返回 ``run.json`` 的 ``levels[i]`` 记录（失败也返回，带 ``error``）。"""
    notes: list[str] = []
    record = level_skeleton(sessions)
    failures: list[str] = []
    env: ProbeEnv | None = None
    shards: list[_ShardHandle] = []
    sampling = _Sampling()
    payloads: list[dict[str, Any]] = []
    shutil.rmtree(level_dir, ignore_errors=True)
    level_dir.mkdir(parents=True, exist_ok=True)
    try:
        env = await _start_env(cfg, sessions, level_dir)
        identity = await _gateway_identity(env)
        record["gateway"].update(
            {"pid": _process_pid(env), "url": env.gateway_url, **identity}
        )
        created = await _create_sessions(env, sessions, level_dir)
        record["session_create"].update(
            {key: created[key] for key in ("ok", "failed", "total_ms")}
        )
        if created["ok"] != sessions:
            raise OrchestratorError(
                f"created only {created['ok']} of {sessions} sessions "
                f"({created['failed']} failed; first failures: {created['failures'][:3]})"
            )
        shards = await _spawn_shards(
            cfg, sessions, created["session_ids"], env, level_dir, notes
        )
        record["shape"].update(
            {"rate": cfg.rate, "shards": len(shards), "subscribers": cfg.subscribers}
        )
        window_start, window_end = await _await_ready(shards, cfg)
        sampling.window_start = window_start
        sampling.window_end = window_end
        try:
            await _sample_window(env, sampling)
        except Exception as exc:  # noqa: BLE001 - 采样失败不阻断收样本
            failures.append(f"sampling: {type(exc).__name__}: {exc}")
            traceback.print_exc()
        payloads = await _collect(shards)
    except Exception as exc:  # noqa: BLE001 - 单档失败不该吞掉其余档
        failures.append(f"{type(exc).__name__}: {exc}")
        traceback.print_exc()
    finally:
        for handle in shards:
            await handle.stop()
        if env is not None:
            await env.stop()
            _keep_log(env, level_dir)

    _assemble(
        record,
        cfg=cfg,
        sessions=sessions,
        provider=None if env is None else env.provider,
        sampling=sampling,
        payloads=payloads,
        notes=notes,
    )
    if failures:
        record["error"] = " | ".join(failures)
        notes.append(f"level {sessions} failed: {record['error']}")
    record["notes"] = notes
    if not failures:
        shutil.rmtree(level_dir / "env", ignore_errors=True)
        shutil.rmtree(level_dir / WORKSPACE_DIRNAME, ignore_errors=True)
    return record


def _process_pid(env: ProbeEnv) -> int | None:
    process = env.process
    return None if process is None else int(process.pid)


async def _gateway_identity(env: ProbeEnv) -> dict[str, Any]:
    """网关身份（版本 / commit）：产物溯源用，缺字段就记 ``None``。"""
    try:
        async with httpx.AsyncClient(
            base_url=env.gateway_url, timeout=10.0, trust_env=False
        ) as client:
            response = await client.get("/api/health")
    except httpx.HTTPError:
        return {"version": None, "commit": None}
    body = response.json() if response.status_code == 200 else {}
    return {
        "version": body.get("version") if isinstance(body, dict) else None,
        "commit": body.get("commit") if isinstance(body, dict) else None,
    }


async def _start_env(cfg: RunConfig, sessions: int, level_dir: Path) -> ProbeEnv:
    """起隔离环境（fresh WING_HOME + 假 Provider + 网关子进程）并注册剧本。"""
    shape = shapes.resolve(cfg.scenario)
    env = await ProbeEnv.start(
        level_dir / "env",
        gateway_bin=resolve_gateway_bin(),
        model=shape.MODEL,
        tools=TOOLS,
        system_prompt=SYSTEM_PROMPT,
        health_timeout=GATEWAY_HEALTH_TIMEOUT,
    )
    env.register(
        shape.MODEL,
        shape.script(sessions=sessions, rate=cfg.rate, span_s=cfg.span_s),
    )
    return env


async def _create_sessions(
    env: ProbeEnv, sessions: int, level_dir: Path
) -> dict[str, Any]:
    """coordinator 经公开 HTTP 建全部会话（记 ok / failed / 耗时）。"""
    workspace = level_dir / WORKSPACE_DIRNAME
    workspace.mkdir(parents=True, exist_ok=True)
    created: list[str] = []
    failures: list[str] = []
    started = time.monotonic()
    limits = httpx.Limits(
        max_connections=CREATE_CONCURRENCY, max_keepalive_connections=CREATE_CONCURRENCY
    )
    async with httpx.AsyncClient(
        base_url=env.gateway_url, timeout=60.0, trust_env=False, limits=limits
    ) as client:
        gate = asyncio.Semaphore(CREATE_CONCURRENCY)

        async def one(index: int) -> None:
            async with gate:
                try:
                    response = await client.post(
                        "/api/session/create", json={"workspace": str(workspace)}
                    )
                except httpx.HTTPError as exc:
                    failures.append(f"#{index}: {exc!r}")
                    return
                if response.status_code != 200:
                    failures.append(
                        f"#{index}: {response.status_code} {response.text[:120]}"
                    )
                    return
                session_id = response.json().get("session_id")
                if not isinstance(session_id, str) or not session_id:
                    failures.append(f"#{index}: response has no session_id")
                    return
                created.append(session_id)

        await asyncio.gather(*(one(index) for index in range(sessions)))
    total_ms = (time.monotonic() - started) * 1000.0
    return {
        "ok": len(created),
        "failed": len(failures),
        "total_ms": round(total_ms, 1),
        "session_ids": created,
        "failures": failures[:8],
    }


async def _spawn_shards(
    cfg: RunConfig,
    sessions: int,
    session_ids: list[str],
    env: ProbeEnv,
    level_dir: Path,
    notes: list[str],
) -> list[_ShardHandle]:
    """轮转切分会话并 spawn 分片子进程（配置 / 样本 / 日志各一份文件）。"""
    count = cfg.shards_for(sessions)
    if count != cfg.shards:
        notes.append(
            f"shards clamped from {cfg.shards} to {count} (sessions={sessions})"
        )
    handles: list[_ShardHandle] = []
    for index in range(count):
        config_path = level_dir / f"shard-{index}.json"
        out_path = level_dir / f"shard-{index}.samples.json"
        log_path = level_dir / f"shard-{index}.log"
        config_path.write_text(
            json.dumps(
                {
                    "shard": index,
                    "gateway_url": env.gateway_url,
                    "sessions": session_ids[index::count],
                    "subscribers": cfg.subscribers,
                    "shape": cfg.scenario,
                    "out": str(out_path),
                },
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )
        log_handle = open(log_path, "wb")
        process = await asyncio.create_subprocess_exec(
            sys.executable,
            str(SHARD_SCRIPT),
            "--config",
            str(config_path),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=log_handle,
            cwd=str(level_dir),
        )
        handles.append(
            _ShardHandle(
                index=index,
                process=process,
                config_path=config_path,
                out_path=out_path,
                log_path=log_path,
                log_handle=log_handle,
            )
        )
    return handles


async def _await_ready(
    handles: list[_ShardHandle], cfg: RunConfig
) -> tuple[float, float]:
    """等全部分片就绪，再算出窗口边界下发给它们。

    窗口起点必须在**所有分片**连上并订阅完之后才算——否则慢机器上的分片会带着
    「窗口已开始」的时钟起跑，读数里混进一段没有负载的窗口。
    """
    for handle in handles:
        await _read_event(handle, expect="ready", timeout=SHARD_READY_TIMEOUT)
    window_start = time.monotonic() + cfg.warmup
    window_end = window_start + cfg.dwell
    message = (
        json.dumps({"window_start": window_start, "window_end": window_end}) + "\n"
    ).encode()
    for handle in handles:
        stdin = handle.process.stdin
        if stdin is None:  # pragma: no cover - create_subprocess_exec 给了 PIPE
            raise OrchestratorError(f"shard {handle.index} has no stdin pipe")
        stdin.write(message)
        await stdin.drain()
    return window_start, window_end


async def _read_event(
    handle: _ShardHandle, *, expect: str, timeout: float
) -> dict[str, Any]:
    """读一行握手 JSON（超时 / 提前退出 / 对端报错都抛，带分片日志尾部）。"""
    stdout = handle.process.stdout
    if stdout is None:  # pragma: no cover - create_subprocess_exec 给了 PIPE
        raise OrchestratorError(f"shard {handle.index} has no stdout pipe")
    try:
        line = await asyncio.wait_for(stdout.readline(), timeout)
    except TimeoutError as exc:
        raise OrchestratorError(
            f"shard {handle.index} did not report {expect!r} within {timeout:.0f}s\n"
            f"{read_log_tail(handle.log_path)}"
        ) from exc
    if not line:
        raise OrchestratorError(
            f"shard {handle.index} exited before reporting {expect!r} "
            f"(code {handle.process.returncode})\n{read_log_tail(handle.log_path)}"
        )
    try:
        payload = json.loads(line)
    except ValueError as exc:
        raise OrchestratorError(
            f"shard {handle.index} wrote a non-JSON handshake line: {line!r}"
        ) from exc
    if not isinstance(payload, dict):
        raise OrchestratorError(
            f"shard {handle.index} handshake line is not a JSON object: {line!r}"
        )
    event = payload.get("event")
    if event == "error":
        raise OrchestratorError(
            f"shard {handle.index} reported an error: {payload.get('message')}\n"
            f"{read_log_tail(handle.log_path)}"
        )
    if event != expect:
        raise OrchestratorError(
            f"shard {handle.index} reported {event!r}, expected {expect!r}: {payload}"
        )
    return payload


async def _sample_window(env: ProbeEnv, sampling: _Sampling) -> None:
    """窗口内采样：CPU 首尾累计差 + RSS/fd 周期峰值 + 网关日志的回收行计数。

    采样原地写进 ``sampling``：中途失败时已采到的点仍留在产物里（崩机现场是容量测量
    最想留住的那一格）。RSS / fd 走线程——darwin 上它们是 ``ps`` / ``lsof`` 子进程
    （合计约 17 ms），同步跑会每秒卡住一次正在产出负载的假 Provider。
    """
    process = env.process
    if process is None:
        raise OrchestratorError("the gateway process is gone before the sample window")
    gateway_pid = int(process.pid)
    sampling.gateway_pid = gateway_pid
    log_path = env.log_path
    delay = sampling.window_start - time.monotonic()
    if delay > 0:
        await asyncio.sleep(delay)
    _require_gateway_alive(process, gateway_pid, log_path)
    sampling.loadavg = _loadavg()
    gateway_cpu0 = proc.process_cpu_seconds(gateway_pid)
    provider_cpu0 = proc.self_cpu_seconds()
    started = time.monotonic()
    while True:
        _require_gateway_alive(process, gateway_pid, log_path)
        rss_mb, fds = await asyncio.to_thread(_sample_memory, gateway_pid)
        sampling.rss_mb.append(rss_mb)
        sampling.fds.append(fds)
        remaining = sampling.window_end - time.monotonic()
        if remaining <= 0:
            break
        await asyncio.sleep(min(SAMPLE_INTERVAL, remaining))
    sampling.wall_s = time.monotonic() - started
    sampling.gateway_cpu_seconds = proc.process_cpu_seconds(gateway_pid) - gateway_cpu0
    sampling.provider_cpu_seconds = proc.self_cpu_seconds() - provider_cpu0


def _sample_memory(pid: int) -> tuple[float, int]:
    """网关的 ``(RSS MB, fd 数)``（阻塞读取，调用方负责放进线程）。"""
    return proc.process_rss_mb(pid), proc.process_fds(pid)


def _require_gateway_alive(
    process: asyncio.subprocess.Process, pid: int, log_path: Path
) -> None:
    """网关必须还活着：死了要留下「网关退出了」这句话，而不是采样器的措辞。"""
    code = process.poll()
    if code is None:
        return
    raise OrchestratorError(
        f"gateway exited during the sample window (pid {pid}, code {code})\n"
        f"{read_log_tail(log_path)}"
    )


async def _collect(handles: list[_ShardHandle]) -> list[dict[str, Any]]:
    """等每个分片报 done / 退出，读回它的样本文件。"""
    payloads: list[dict[str, Any]] = []
    for handle in handles:
        await _read_event(handle, expect="done", timeout=SHARD_EXIT_TIMEOUT)
        try:
            code = await asyncio.wait_for(handle.process.wait(), SHARD_EXIT_TIMEOUT)
        except TimeoutError as exc:
            raise OrchestratorError(
                f"shard {handle.index} did not exit within {SHARD_EXIT_TIMEOUT:.0f}s "
                f"of reporting done\n{read_log_tail(handle.log_path)}"
            ) from exc
        if code != 0:
            raise OrchestratorError(
                f"shard {handle.index} exited with code {code}\n"
                f"{read_log_tail(handle.log_path)}"
            )
        try:
            payload = json.loads(handle.out_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise OrchestratorError(
                f"cannot read samples from shard {handle.index} ({handle.out_path}): {exc}"
            ) from exc
        if not isinstance(payload, dict):
            raise OrchestratorError(
                f"shard {handle.index} sample file is not a JSON object"
            )
        payloads.append(payload)
    return payloads


# ── 指标装配 ──────────────────────────────────────────────────


def _assemble(
    record: dict[str, Any],
    *,
    cfg: RunConfig,
    sessions: int,
    provider: Any,
    sampling: _Sampling,
    payloads: list[dict[str, Any]],
    notes: list[str],
) -> dict[str, Any]:
    """把采样与分片样本折进 ``record``（键集在 :func:`level_skeleton` 就定好了）。"""
    window_start = sampling.window_start
    turns = [
        TurnSample(
            prompt=str(entry.get("prompt", "")),
            sent_at=window_start + int(entry["sent_us"]) / 1e6,
            result_at=window_start + int(entry["result_us"]) / 1e6,
            arrivals=tuple(
                window_start + int(value) / 1e6
                for value in entry.get("arrivals_us", ())
            ),
        )
        for payload in payloads
        for entry in payload.get("turns", ())
    ]
    text_frames = sum(int(payload.get("text_frames", 0)) for payload in payloads)
    errors = sum(int(payload.get("errors", 0)) for payload in payloads)
    wall = sampling.wall_s
    block = record["metrics"]
    samples = record["samples"]
    samples["text_frames"] = text_frames if payloads else None
    samples["turns"] = len(turns) if payloads else None
    record["wall_s"] = round(wall, 3) if wall > 0 else None

    block["gateway.cpu_cores"] = _ratio(sampling.gateway_cpu_seconds, wall)
    if sampling.rss_mb:
        block["gateway.rss_mb_peak"] = round(max(sampling.rss_mb), 1)
        block["gateway.rss_mb_end"] = round(sampling.rss_mb[-1], 1)
    if sampling.fds:
        block["gateway.fds_peak"] = max(sampling.fds)
    block["slow_client_recycles"] = (
        sum(int(payload.get("closed_before_end", 0)) for payload in payloads)
        if payloads
        else None
    )

    if payloads:
        block["errors"] = errors
        if text_frames > 0 and wall > 0:
            block["tokens_per_second_actual"] = round(text_frames / wall, 1)
        else:
            notes.append(
                "tokens_per_second_actual is null: no text frame arrived in the window"
            )
    turn_spread = metrics.spread(
        [(turn.result_at - turn.sent_at) * 1000.0 for turn in turns]
    )
    block["turn.p50_ms"] = _round(turn_spread.get("p50"))
    block["turn.p99_ms"] = _round(turn_spread.get("p99"))

    delivery_available = provider is not None and metrics.has_frame_emits(provider)
    samples["delivery_available"] = None if provider is None else delivery_available
    if delivery_available and turns:
        pairing = metrics.pair_delivery(provider, provider.requests.all(), turns)
        for label, value in metrics.spread(pairing.latencies_ms).items():
            block[f"delivery.{label}_ms"] = _round(value)
        samples["count"] = pairing.paired
        samples["dropped"] = pairing.dropped
        samples["dropped_reasons"] = dict(pairing.reasons)
        if pairing.reasons:
            notes.append(
                f"delivery pairing dropped {pairing.dropped} sample(s): "
                f"{dict(pairing.reasons)}"
                + (
                    " — the emitter clock must cover content frames only"
                    " (one emit per client `text` event)"
                    if "length_mismatch" in pairing.reasons
                    else ""
                )
            )
        notes.append(
            "delivery percentile basis: nearest-rank over "
            f"{pairing.paired} paired frame(s)"
        )
    elif provider is not None and not delivery_available:
        notes.append(DELIVERY_UNAVAILABLE)
    elif provider is not None:
        notes.append("delivery.* is null: no turn completed inside the sample window")

    shard_cpu: list[float | None] = []
    for payload in payloads:
        shard_cpu.append(_shard_cpu_cores(payload, wall, notes))
        notes.extend((payload.get("error_details") or ())[:2])
    record["rig"].update(
        {
            "provider_cpu_cores": _ratio(sampling.provider_cpu_seconds, wall),
            "shard_cpu_cores": shard_cpu,
            "host_loadavg": sampling.loadavg,
        }
    )
    expected_shards = record["shape"]["shards"]
    if payloads and expected_shards not in (None, len(payloads)):
        notes.append(
            f"only {len(payloads)} of {expected_shards} shard(s) reported samples"
        )
    if record["shape"]["subscribers"] is None:
        record["shape"].update(
            {
                "rate": cfg.rate,
                "shards": len(payloads) or None,
                "subscribers": cfg.subscribers,
            }
        )
    return record


def _shard_cpu_cores(
    payload: dict[str, Any], wall: float, notes: list[str]
) -> float | None:
    """分片自报的窗口内 CPU 秒 → 平均核数（窗口长度用分片自己的记账区间）。"""
    span = payload.get("cpu_span_s")
    divisor = float(span) if isinstance(span, (int, float)) and span > 0 else wall
    if (
        isinstance(span, (int, float))
        and wall > 0
        and abs(span - wall) > CPU_WINDOW_TOLERANCE * wall
    ):
        notes.append(
            f"shard {payload.get('shard')} CPU window {span:.2f}s differs from the "
            f"sample window {wall:.2f}s (numerator/denominator mismatch)"
        )
    return _ratio(payload.get("cpu_seconds"), divisor)


# ── 小工具 ────────────────────────────────────────────────────


def _ratio(seconds: float | None, wall: float) -> float | None:
    """累计 CPU 秒 → 窗口内的平均核数（取不到就 ``None``，不写 0）。"""
    if seconds is None or wall <= 0:
        return None
    return round(seconds / wall, 3)


def _round(value: float | None) -> float | None:
    return None if value is None else round(value, 3)


def _loadavg() -> list[float] | None:
    try:
        return [round(value, 2) for value in os.getloadavg()]
    except OSError:  # pragma: no cover - 少数平台没有 loadavg
        return None


def _keep_log(env: ProbeEnv, level_dir: Path) -> None:
    """把网关日志留在档位目录（``env/`` 成功时会被删掉，日志要留作取证）。"""
    try:
        shutil.copyfile(env.log_path, level_dir / "gateway.log")
    except OSError:  # pragma: no cover - 日志没生成
        pass


__all__ = [
    "CPU_WINDOW_TOLERANCE",
    "DELIVERY_UNAVAILABLE",
    "METRIC_KEYS",
    "OrchestratorError",
    "RunConfig",
    "SAMPLE_KEYS",
    "level_skeleton",
    "run_level",
]
