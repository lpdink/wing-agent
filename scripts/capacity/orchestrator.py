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

#: ``run.json`` 的冻结指标名（顺序即报告列序）。取不到的一律 ``None`` → JSON ``null``。
METRIC_KEYS: tuple[str, ...] = (
    "gateway.cpu_cores",
    "gateway.rss_mb_peak",
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

#: 网关 ``_recycle_client`` 的收尾日志判别词（慢 / 死消费者回收）。
RECYCLE_MARKERS: tuple[str, ...] = (
    "Client disconnected:",
    "(write timeout after",
    "(send failed:",
)

DELIVERY_UNAVAILABLE = (
    "delivery.* is null: frame_emits not available (the fake provider does not "
    "record per-frame emit times)"
)


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
    """窗口内的原始采样结果。"""

    window_start: float
    window_end: float
    wall_s: float
    gateway_pid: int | None = None
    gateway_cpu_seconds: float | None = None
    provider_cpu_seconds: float | None = None
    rss_mb: list[float] = field(default_factory=list)
    fds: list[int] = field(default_factory=list)
    loadavg: list[float] | None = None
    recycles: int | None = None


# ── 一档 ──────────────────────────────────────────────────────


async def run_level(cfg: RunConfig, sessions: int, level_dir: Path) -> dict[str, Any]:
    """跑一档并返回 ``run.json`` 的 ``levels[i]`` 记录（失败也返回，带 ``error``）。"""
    notes: list[str] = []
    try:
        record = await _run_level(cfg, sessions, level_dir, notes)
    except Exception as exc:  # noqa: BLE001 - 单档失败不该吞掉其余档
        traceback.print_exc()
        record = _empty_level(sessions, f"{type(exc).__name__}: {exc}")
        notes.append(f"level {sessions} failed: {record['error']}")
    record["notes"] = notes
    return record


async def _run_level(
    cfg: RunConfig, sessions: int, level_dir: Path, notes: list[str]
) -> dict[str, Any]:
    shape = shapes.resolve(cfg.scenario)
    shutil.rmtree(level_dir, ignore_errors=True)
    level_dir.mkdir(parents=True, exist_ok=True)
    env = await _start_env(cfg, shape, sessions, level_dir)
    identity = await _gateway_identity(env)
    shards: list[_ShardHandle] = []
    try:
        created = await _create_sessions(env, sessions, level_dir)
        if created["ok"] != sessions:
            raise OrchestratorError(
                f"created only {created['ok']} of {sessions} sessions "
                f"({created['failed']} failed; first failures: {created['failures'][:3]})"
            )
        shards = await _spawn_shards(
            cfg, sessions, created["session_ids"], env, level_dir, notes
        )
        window_start, window_end = await _await_ready(shards, cfg)
        sampling = await _sample_window(env, window_start, window_end)
        payloads = await _collect(shards, window_start)
    finally:
        for handle in shards:
            await handle.stop()
        await env.stop()
        _keep_log(env, level_dir)

    record = _assemble(cfg, sessions, env, identity, created, sampling, payloads, notes)
    shutil.rmtree(level_dir / "env", ignore_errors=True)
    return record


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


async def _start_env(
    cfg: RunConfig, shape: Any, sessions: int, level_dir: Path
) -> ProbeEnv:
    """起隔离环境（fresh WING_HOME + 假 Provider + 网关子进程）并注册剧本。"""
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


async def _sample_window(
    env: ProbeEnv, window_start: float, window_end: float
) -> _Sampling:
    """窗口内采样：CPU 首尾累计差 + RSS/fd 周期峰值 + 网关日志的回收行计数。"""
    process = env.process
    if process is None:
        raise OrchestratorError("the gateway process is gone before the sample window")
    gateway_pid = int(process.pid)
    log_path = env.log_path
    delay = window_start - time.monotonic()
    if delay > 0:
        await asyncio.sleep(delay)
    log_offset = _file_size(log_path)
    sampling = _Sampling(
        window_start=window_start,
        window_end=window_end,
        wall_s=0.0,
        gateway_pid=gateway_pid,
    )
    started = time.monotonic()
    gateway_cpu0 = proc.process_cpu_seconds(gateway_pid)
    provider_cpu0 = proc.self_cpu_seconds()
    sampling.loadavg = _loadavg()
    while True:
        sampling.rss_mb.append(proc.process_rss_mb(gateway_pid))
        sampling.fds.append(proc.process_fds(gateway_pid))
        remaining = window_end - time.monotonic()
        if remaining <= 0:
            break
        await asyncio.sleep(min(SAMPLE_INTERVAL, remaining))
    sampling.wall_s = time.monotonic() - started
    sampling.gateway_cpu_seconds = proc.process_cpu_seconds(gateway_pid) - gateway_cpu0
    sampling.provider_cpu_seconds = proc.self_cpu_seconds() - provider_cpu0
    sampling.recycles = _count_recycles(log_path, log_offset)
    return sampling


async def _collect(
    handles: list[_ShardHandle], window_start: float
) -> list[dict[str, Any]]:
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
    cfg: RunConfig,
    sessions: int,
    env: ProbeEnv,
    identity: dict[str, Any],
    created: dict[str, Any],
    sampling: _Sampling,
    payloads: list[dict[str, Any]],
    notes: list[str],
) -> dict[str, Any]:
    """把采样与分片样本折成 ``run.json`` 的一档记录。"""
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
    metrics_block: dict[str, Any] = dict.fromkeys(METRIC_KEYS)

    metrics_block["gateway.cpu_cores"] = _ratio(sampling.gateway_cpu_seconds, wall)
    if sampling.rss_mb:
        metrics_block["gateway.rss_mb_peak"] = round(max(sampling.rss_mb), 1)
        metrics_block["gateway.rss_mb_end"] = round(sampling.rss_mb[-1], 1)
    if sampling.fds:
        metrics_block["gateway.fds_peak"] = max(sampling.fds)
    metrics_block["tokens_per_second_actual"] = (
        round(text_frames / wall, 1) if wall > 0 else None
    )
    metrics_block["errors"] = errors
    metrics_block["slow_client_recycles"] = sampling.recycles

    turn_spread = metrics.spread(
        [(turn.result_at - turn.sent_at) * 1000.0 for turn in turns]
    )
    metrics_block["turn.p50_ms"] = _round(turn_spread.get("p50"))
    metrics_block["turn.p99_ms"] = _round(turn_spread.get("p99"))

    delivery_available = metrics.has_frame_emits(env.provider)
    samples: dict[str, Any] = {
        "count": None,
        "dropped": None,
        "text_frames": text_frames,
        "turns": len(turns),
        "delivery_available": delivery_available,
    }
    if delivery_available and turns:
        pairing = metrics.pair_delivery(
            env.provider, env.provider.requests.all(), turns
        )
        delivery = metrics.spread(pairing.latencies_ms)
        for label, value in delivery.items():
            metrics_block[f"delivery.{label}_ms"] = _round(value)
        samples["count"] = pairing.paired
        samples["dropped"] = pairing.dropped
        samples["dropped_reasons"] = {}
        if pairing.reasons:
            samples["dropped_reasons"] = dict(pairing.reasons)
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
    elif not delivery_available:
        notes.append(DELIVERY_UNAVAILABLE)
    else:
        notes.append("delivery.* is null: no turn completed inside the sample window")

    shard_cpu = [_ratio(payload.get("cpu_seconds"), wall) for payload in payloads]
    rig = {
        "provider_cpu_cores": _ratio(sampling.provider_cpu_seconds, wall),
        "shard_cpu_cores": shard_cpu,
        "host_loadavg": sampling.loadavg,
    }
    if any(payload.get("error_details") for payload in payloads):
        notes.extend(
            detail
            for payload in payloads
            for detail in payload.get("error_details", ())[:2]
        )
    record: dict[str, Any] = {
        "sessions": sessions,
        "wall_s": round(wall, 3),
        "metrics": metrics_block,
        "rig": rig,
        "samples": samples,
        "session_create": {
            "ok": created["ok"],
            "failed": created["failed"],
            "total_ms": created["total_ms"],
        },
        "gateway": {
            "pid": sampling.gateway_pid,
            "url": env.gateway_url,
            "version": identity.get("version"),
            "commit": identity.get("commit"),
        },
        "shape": {
            "rate": cfg.rate,
            "shards": len(payloads),
            "subscribers": cfg.subscribers,
        },
    }
    if record["shape"]["shards"] != cfg.shards_for(sessions):
        notes.append(
            f"only {record['shape']['shards']} of "
            f"{cfg.shards_for(sessions)} shard(s) reported samples"
        )
    return record


def _empty_level(sessions: int, error: str) -> dict[str, Any]:
    """失败档的形状：指标全 ``null``（不写 0），错误信息进 ``error``。"""
    return {
        "sessions": sessions,
        "wall_s": None,
        "metrics": dict.fromkeys(METRIC_KEYS),
        "rig": {
            "provider_cpu_cores": None,
            "shard_cpu_cores": [],
            "host_loadavg": None,
        },
        "samples": {
            "count": None,
            "dropped": None,
            "text_frames": None,
            "turns": None,
            "delivery_available": None,
        },
        "session_create": {"ok": None, "failed": None, "total_ms": None},
        "gateway": {"pid": None, "url": None},
        "error": error,
    }


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


def _file_size(path: Path) -> int:
    try:
        return path.stat().st_size
    except OSError:
        return 0


def _count_recycles(path: Path, offset: int) -> int | None:
    """窗口内的慢 / 死消费者回收行数（读不到日志 → ``None``）。"""
    try:
        with path.open("rb") as handle:
            handle.seek(offset)
            text = handle.read().decode("utf-8", "replace")
    except OSError:
        return None
    return sum(1 for line in text.splitlines() if _is_recycle_line(line))


def _is_recycle_line(line: str) -> bool:
    """网关的回收日志形如 ``Client disconnected: <id> (write timeout after 30s)``。"""
    return RECYCLE_MARKERS[0] in line and any(
        marker in line for marker in RECYCLE_MARKERS[1:]
    )


def _keep_log(env: ProbeEnv, level_dir: Path) -> None:
    """把网关日志留在档位目录（``env/`` 成功时会被删掉，日志要留作取证）。"""
    try:
        shutil.copyfile(env.log_path, level_dir / "gateway.log")
    except OSError:  # pragma: no cover - 日志没生成
        pass


__all__ = [
    "DELIVERY_UNAVAILABLE",
    "METRIC_KEYS",
    "OrchestratorError",
    "RunConfig",
    "run_level",
]
