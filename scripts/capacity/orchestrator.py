#!/usr/bin/env python3
"""一次容量测量的**一档**：进程拓扑、会话建立、采样窗口、样本回收。

拓扑（为什么这样分层、如何避免测出假的退化 → ``README.md``「进程拓扑」）：

    coordinator（本模块，run.py 驱动）
    ├── 假 Provider：同进程内 aiohttp（``wing_probe.provider.FakeProvider``）
    ├── 网关：独立子进程（``wing_probe.env.ProbeEnv``，fresh WING_HOME + OS 分配端口）
    └── driver 分片 ×K：独立子进程（``driver_shard.py``）

一档的生命周期：起环境 → 注册剧本 → coordinator 经 HTTP 建全部会话 →（``--history-tokens``
时先预置历史）→（水合形态重启网关）→ spawn 分片并等全部就绪 → 下发窗口
（``window_start/end`` 是绝对 ``time.monotonic()``）→ 窗口内采样 →（逐出形态确认逐出）
→ 收分片样本 → 拆环境（异常路径同样收干净）。建会话由 coordinator 统一做：
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
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, Any

import httpx
from wing_probe.env import ProbeEnv, read_log_tail, resolve_gateway_bin

import metrics
import proc
import shapes
from metrics import TurnSample
from verdict import VERDICT_KEYS

#: 分片就绪 / 退出的等待上限（秒）。
SHARD_READY_TIMEOUT = 180.0
SHARD_EXIT_TIMEOUT = 60.0
SHARD_DRAIN_TIMEOUT = 300.0
"""``done`` 握手落空后继续等样本文件的上限（秒）。

分片是**先写样本、再报 done**；一档把网关压到饱和时（池打开 + 长阶梯），窗口之后仍有
几十万帧堵在 socket 里，分片收尾可能比 60 s 的握手窗口更久。样本已在盘上就是完整读数，
为一行握手把它丢掉（连带整档变成 ``error``）是本末倒置。
"""

#: 建会话 / 切模型 / 逐出探针的并发上限（低于 httpx 默认连接池，避免把「建会话」自己变成瓶颈）。
CREATE_CONCURRENCY = 32

#: 网关健康检查上限（秒）：本档要建 N 个会话，冷启动慢机器也够。
GATEWAY_HEALTH_TIMEOUT = 120.0

#: 历史预置的等待上限（秒）：20000 token × 1000 会话是分钟级的活。
SEED_TIMEOUT = 900.0

SYSTEM_PROMPT = "You are wing's capacity probe agent. Answer briefly."

WORKSPACE_DIRNAME = "workspace"
SEED_DIRNAME = "seed"
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
    "hydration.p50_ms",
    "hydration.p99_ms",
    "hydration.p999_ms",
    "first_frame.p50_ms",
    "first_frame.p99_ms",
    "first_frame.p999_ms",
    "tokens_per_second_actual",
    "events_per_second_actual",
    "turns_per_second",
    "chars_per_second",
    "errors",
    "slow_client_recycles",
)

#: ``run.json`` 的样本记账键（成功档与失败档键集相同，缺数据是 ``None``）。
SAMPLE_KEYS: tuple[str, ...] = (
    "count",
    "dropped",
    "dropped_reasons",
    "delta_frames",
    "paired_channels",
    "text_frames",
    "chars",
    "turns",
    "turns_in_flight",
    "delivery_available",
    "sync_sessions",
    "hydration_attempts",
    "hydration_failed",
    "evicted_by_reaper",
    "released_by_probe",
)

#: 历史预置的记账键（没预置时全 ``None``，不是 0）。
HISTORY_KEYS: tuple[str, ...] = (
    "tokens_per_session",
    "chars_per_session",
    "turns_per_session",
    "total_ms",
    "failed",
)

DELIVERY_UNAVAILABLE = (
    "delivery.* is null: frame_emits not available (the fake provider does not "
    "record per-frame emit times)"
)

#: 分片自报的 CPU 窗口与 coordinator 的采样窗口允许的相对偏差（超过即记 note）。
CPU_WINDOW_TOLERANCE = 0.1


class OrchestratorError(RuntimeError):
    """一档无法继续（网关起不来 / 分片崩溃 / 会话建不全）——本档记失败并继续。"""


class HandshakeTimeout(OrchestratorError):
    """分片没在期限内报 ``done``（样本可能仍在盘上，见 ``_await_samples``）。"""


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
    chunk: int = shapes.DEFAULT_CHUNK
    backend: str = "file"
    tool: str = shapes.DEFAULT_TOOL
    cycle_s: float = shapes.DEFAULT_CYCLE_S
    decode_duty: float = shapes.DEFAULT_DECODE_DUTY
    provider_max_connections: int | None = None
    """``--provider-max-connections``：``None`` = 不写该键（用网关自己的默认值）。"""

    def __post_init__(self) -> None:
        # 产物目录必须绝对：分片以 ``cwd=<档位目录>`` 起、拿到的配置路径由它派生，
        # 相对路径会让子进程找不到自己的配置。规范化放在这里，任何构造路径都安全。
        object.__setattr__(self, "out", Path(self.out).expanduser().resolve())

    @property
    def span_s(self) -> float:
        """每档的驱动时长（warmup + dwell）。"""
        return self.warmup + self.dwell

    def shards_for(self, sessions: int) -> int:
        """本档实际分片数（分片多于会话是纯浪费）。"""
        return max(1, min(self.shards, sessions))


def level_args(
    cfg: RunConfig, sessions: int, *, frame_delay_s: float | None = None
) -> shapes.ShapeArgs:
    """``RunConfig`` + 档位信息 → 形态参数（CLI 校验与本模块装配共用一处拼装）。"""
    return shapes.ShapeArgs(
        scenario=cfg.scenario,
        sessions=sessions,
        rate=cfg.rate,
        span_s=cfg.span_s,
        dwell_s=cfg.dwell,
        chunk=cfg.chunk,
        cycle_s=cfg.cycle_s,
        decode_duty=cfg.decode_duty,
        tool=cfg.tool,
        history_tokens=cfg.history_tokens,
        frame_delay_s=frame_delay_s,
    )


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
        "history": dict.fromkeys(HISTORY_KEYS),
        "session_create": {"ok": None, "failed": None, "total_ms": None},
        "gateway": {
            "pid": None,
            "url": None,
            "version": None,
            "commit": None,
            "restart_ms": None,
        },
        "shape": {
            "rate": None,
            "chunk": None,
            "backend": None,
            "tool": None,
            "shards": None,
            "subscribers": None,
            "window_mode": None,
            "drives_upstream": None,
            "turn_boundaries": None,
        },
        "verdict": dict.fromkeys(VERDICT_KEYS),
        "error": None,
        "error_detail": None,
    }


async def run_level(
    cfg: RunConfig,
    sessions: int,
    level_dir: Path,
    *,
    frame_delay_s: float | None = None,
    tracebacks: bool = True,
) -> dict[str, Any]:
    """跑一档并返回 ``run.json`` 的 ``levels[i]`` 记录（失败也返回，带 ``error``）。"""
    notes: list[str] = []
    record = level_skeleton(sessions)
    failures: list[str] = []
    env: ProbeEnv | None = None
    shards: list[_ShardHandle] = []
    sampling = _Sampling()
    payloads: list[dict[str, Any]] = []
    plan: shapes.ShapePlan | None = None
    session_ids: list[str] = []
    shutil.rmtree(level_dir, ignore_errors=True)
    level_dir.mkdir(parents=True, exist_ok=True)
    try:
        plan = shapes.build_plan(level_args(cfg, sessions, frame_delay_s=frame_delay_s))
        record["shape"].update(
            {
                "rate": cfg.rate,
                "chunk": cfg.chunk,
                "backend": cfg.backend,
                "tool": cfg.tool,
                "subscribers": cfg.subscribers,
                "window_mode": plan.window_mode,
                "drives_upstream": plan.drives_upstream,
                "turn_boundaries": plan.turn_boundaries,
            }
        )
        env = await _start_env(cfg, level_dir, plan)
        identity = await _gateway_identity(env)
        record["gateway"].update(
            {"pid": _process_pid(env), "url": env.gateway_url, **identity}
        )
        _write_workspace_files(level_dir / WORKSPACE_DIRNAME, plan)
        created = await _create_sessions(env, sessions, level_dir, plan, cfg)
        session_ids = list(created["session_ids"])
        record["session_create"].update(
            {key: created[key] for key in ("ok", "failed", "total_ms")}
        )
        if created["ok"] != sessions:
            raise OrchestratorError(
                f"created only {created['ok']} of {sessions} sessions "
                f"({created['failed']} failed; first failures: {created['failures'][:3]})"
            )
        if plan.seed is not None:
            await _seed_history(cfg, env, session_ids, level_dir, plan, record)
            await _switch_models(env, session_ids, plan)
        if plan.window_mode == "hydrate":
            started = time.monotonic()
            await env.restart_gateway()
            record["gateway"]["restart_ms"] = round(
                (time.monotonic() - started) * 1000.0, 1
            )
            identity = await _gateway_identity(env)
            record["gateway"].update(
                {"pid": _process_pid(env), "url": env.gateway_url, **identity}
            )
        await _spawn_shards(cfg, session_ids, env, level_dir, notes, shards, plan)
        record["shape"]["shards"] = len(shards)
        window_start, window_end = await _await_ready(shards, cfg)
        sampling.window_start = window_start
        sampling.window_end = window_end
        try:
            await _sample_window(env, sampling, plan.sample_interval_s)
        except Exception as exc:  # noqa: BLE001 - 采样失败不阻断收样本
            failures.append(f"sampling: {type(exc).__name__}: {exc}")
            if tracebacks:
                traceback.print_exc()
        payloads, shard_problems = await _collect(shards)
        failures.extend(shard_problems)
        if plan.measure_eviction:
            await _probe_eviction(env, session_ids, record, notes)
    except Exception as exc:  # noqa: BLE001 - 单档失败不该吞掉其余档
        failures.append(f"{type(exc).__name__}: {exc}")
        if tracebacks:
            traceback.print_exc()
    finally:
        for handle in shards:
            await handle.stop()
        if env is not None:
            await env.stop()
            _keep_log(env, level_dir)

    try:
        _assemble(
            record,
            cfg=cfg,
            sessions=sessions,
            provider=None if env is None else env.provider,
            sampling=sampling,
            payloads=payloads,
            notes=notes,
        )
    except Exception as exc:  # noqa: BLE001 - 装配失败也要留下其余档与已有数据
        failures.append(f"assemble: {type(exc).__name__}: {exc}")
        if tracebacks:
            traceback.print_exc()
    if failures:
        # 一句话进冻结字段 ``error``（给机器读与报告横幅），完整诊断进 ``error_detail``
        # （网关日志尾部有几千字符，重复三遍会把 run.json 淹掉）。
        record["error"] = " | ".join(_first_line(item) for item in failures)[:400]
        record["error_detail"] = failures
    record["notes"] = notes
    if not failures:
        for scratch in ("env", WORKSPACE_DIRNAME, SEED_DIRNAME):
            shutil.rmtree(level_dir / scratch, ignore_errors=True)
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


async def measure_pacing(rate: float) -> float:
    """实测本机 ``asyncio.sleep`` 在帧间隔上的系统性超时（秒）。

    Provider 的注入节奏是「睡 ``delay`` 再写一帧」，而 ``asyncio.sleep`` 在这里（以及
    多数平台）会稳定地多睡一点——本机约 2.1 ms、与 ``delay`` 无关。不补偿的话
    ``--rate`` 只是**注入参数**而不是实际速率（50 事件/s 会跑成 45.6）。这里在档位开始
    前空载测一次，把中位数作为补偿量交给形态；实际达到的速率仍以 ``events_per_second_*``
    为准（补偿的残差在报告里看得见）。
    """
    interval = 1.0 / rate
    samples = max(10, min(40, int(1.0 / interval)))
    overshoot: list[float] = []
    for _ in range(samples):
        started = time.monotonic()
        await asyncio.sleep(interval)
        overshoot.append((time.monotonic() - started) - interval)
    overshoot.sort()
    return overshoot[len(overshoot) // 2]


async def _start_env(
    cfg: RunConfig, level_dir: Path, plan: shapes.ShapePlan
) -> ProbeEnv:
    """起隔离环境（fresh WING_HOME + 假 Provider + 网关子进程）并注册剧本。"""
    shape = shapes.resolve(cfg.scenario)
    provider_extra = (
        None
        if cfg.provider_max_connections is None
        else {"max_connections": cfg.provider_max_connections}
    )
    env = await ProbeEnv.start(
        level_dir / "env",
        gateway_bin=resolve_gateway_bin(),
        model=shape.MODEL,
        models=shapes.model_ids(plan),
        tools=plan.tools,
        system_prompt=SYSTEM_PROMPT,
        sessions=plan.env_sessions,
        health_timeout=GATEWAY_HEALTH_TIMEOUT,
        provider_extra=provider_extra,
    )
    shapes.register(env, plan)
    return env


def _write_workspace_files(workspace: Path, plan: shapes.ShapePlan) -> None:
    """会话 workspace 里的预置文件（工具形态的 ``Read`` 参数指向它们）。"""
    workspace.mkdir(parents=True, exist_ok=True)
    for name, content in plan.workspace_files.items():
        (workspace / name).write_text(content, encoding="utf-8")


async def _post_many(
    env: ProbeEnv,
    path: str,
    payloads: Sequence[dict[str, Any]],
    *,
    timeout: float = 60.0,
) -> list[httpx.Response | str]:
    """对 ``payloads`` 逐个 POST ``path``（限并发），返回与输入**等长、同序**的结果。

    每项要么是 ``httpx.Response``，要么是一条失败描述字符串——调用方只解释它自己那个
    端点的响应语义（建会话要 session_id、切模型只看状态码、逐出看 ``released``）。
    结果按输入下标落位，所以「第 i 个会话用第 i 个 model / 第 i 个 session_id」这类
    映射不需要在调用方重建。
    """
    limits = httpx.Limits(
        max_connections=CREATE_CONCURRENCY, max_keepalive_connections=CREATE_CONCURRENCY
    )
    gate = asyncio.Semaphore(CREATE_CONCURRENCY)
    results: list[Any] = [""] * len(payloads)
    async with httpx.AsyncClient(
        base_url=env.gateway_url, timeout=timeout, trust_env=False, limits=limits
    ) as client:

        async def one(index: int) -> None:
            async with gate:
                try:
                    results[index] = await client.post(path, json=payloads[index])
                except httpx.HTTPError as exc:
                    results[index] = f"#{index}: {exc!r}"

        await asyncio.gather(*(one(index) for index in range(len(payloads))))
    return results


async def _create_sessions(
    env: ProbeEnv,
    sessions: int,
    level_dir: Path,
    plan: shapes.ShapePlan,
    cfg: RunConfig,
) -> dict[str, Any]:
    """coordinator 经公开 HTTP 建全部会话（记 ok / failed / 耗时）。

    请求体带上 ``backend``（file / memory 的 A/B 靠它区分）与 create 期的 agent 覆盖
    （形态的 ``yolo``，以及「先挂到预置 model 上」的 ``model_id``）。
    """
    workspace = level_dir / WORKSPACE_DIRNAME
    workspace.mkdir(parents=True, exist_ok=True)
    payloads: list[dict[str, Any]] = []
    for index in range(sessions):
        agent = dict(plan.agent)
        agent["model_id"] = (
            plan.seed.model if plan.seed is not None else plan.session_models[index]
        )
        payloads.append(
            {
                "workspace": str(workspace),
                "backend": cfg.backend,
                "agent": agent,
            }
        )
    started = time.monotonic()
    created: list[str] = []
    failures: list[str] = []
    for index, result in enumerate(
        await _post_many(env, "/api/session/create", payloads)
    ):
        if isinstance(result, str):
            failures.append(result)
            continue
        if result.status_code != 200:
            failures.append(f"#{index}: {result.status_code} {result.text[:120]}")
            continue
        session_id = result.json().get("session_id")
        if not isinstance(session_id, str) or not session_id:
            failures.append(f"#{index}: response has no session_id")
            continue
        created.append(session_id)
    total_ms = (time.monotonic() - started) * 1000.0
    return {
        "ok": len(created),
        "failed": len(failures),
        "total_ms": round(total_ms, 1),
        "session_ids": created,
        "failures": failures[:8],
    }


async def _seed_history(
    cfg: RunConfig,
    env: ProbeEnv,
    session_ids: Sequence[str],
    level_dir: Path,
    plan: shapes.ShapePlan,
    record: dict[str, Any],
) -> None:
    """预置历史：一个 seed 分片驱动每会话一轮大输出（``send`` → ``turn_result``）。

    走公开路径（订阅 + WS 驱动），落盘的因此是产品自己的规范形。预置耗时是被测事实
    （``--history-tokens 20000`` × 1000 会话是分钟级的），照实记进 ``levels[i].history``。
    """
    seed = plan.seed
    assert seed is not None  # 调用点由 plan.seed 判定
    started = time.monotonic()
    handle = await _spawn_shard(
        index=0,
        sessions=tuple(session_ids),
        mode="seed",
        seed_turns=1,
        env=env,
        level_dir=level_dir / SEED_DIRNAME,
        subscribers="per-shard",
        shape=cfg.scenario,
    )
    failures: list[str] = []
    payload: dict[str, Any] | None = None
    try:
        await _read_event(handle, expect="ready", timeout=SHARD_READY_TIMEOUT)
        message = (
            json.dumps(
                {
                    "window_start": time.monotonic(),
                    "window_end": time.monotonic() + SEED_TIMEOUT,
                }
            )
            + "\n"
        ).encode()
        stdin = handle.process.stdin
        if stdin is None:  # pragma: no cover - create_subprocess_exec 给了 PIPE
            raise OrchestratorError("seed shard has no stdin pipe")
        stdin.write(message)
        await stdin.drain()
        await _read_event(handle, expect="done", timeout=SEED_TIMEOUT)
        payload = json.loads(handle.out_path.read_text(encoding="utf-8"))
        metrics.check_shard_payload(payload)
    except Exception as exc:  # noqa: BLE001 - 预置失败即本档失败（历史是形态的前提）
        failures.append(f"history seed: {type(exc).__name__}: {exc}")
    finally:
        await handle.stop()
    if failures:
        raise OrchestratorError(" | ".join(failures))
    record["history"].update(
        {
            "tokens_per_session": seed.tokens_per_session,
            "chars_per_session": seed.chars_per_session,
            "turns_per_session": 1,
            "total_ms": round((time.monotonic() - started) * 1000.0, 1),
            "failed": int(payload["errors"]) if payload is not None else None,
        }
    )


async def _switch_models(
    env: ProbeEnv, session_ids: Sequence[str], plan: shapes.ShapePlan
) -> None:
    """预置完把每个会话切回形态自己的 model（预置轮与测量轮各有独立剧本游标）。"""
    payloads = [
        {"session_id": session_id, "model_id": plan.session_models[index]}
        for index, session_id in enumerate(session_ids)
    ]
    problems: list[str] = []
    for index, result in enumerate(
        await _post_many(env, "/api/session/update", payloads)
    ):
        if isinstance(result, str):
            problems.append(result)
            continue
        if result.status_code != 200:
            problems.append(
                f"{payloads[index]['session_id']}: {result.status_code} "
                f"{result.text[:120]}"
            )
    if problems:
        # 切不回去 = 测量轮会落到预置剧本上（一轮巨大输出），读数彻底失真。
        raise OrchestratorError(
            f"cannot switch {len(problems)} session(s) to their measuring model: "
            f"{problems[:3]}"
        )


async def _probe_eviction(
    env: ProbeEnv,
    session_ids: Sequence[str],
    record: dict[str, Any],
    notes: list[str],
) -> None:
    """窗口结束后确认 reaper 是否真的回收了会话（逐出形态的唯一直接证据）。

    ``POST /api/session/release`` 是公开路径且幂等：``released=false``
    （detail=``not loaded``）意味着会话早已不在内存——即 reaper 在窗口里把它收走了。
    非持久后端会被 409 拒（产品禁止销毁内存会话），此时整组写 ``null`` 并点名，不写 0。

    两个计数是**窗口末的时刻**（探针紧接着采样窗口跑），不含逐出发生的时刻；
    RSS 落差**不是**逐出的证据——进程被清空后分配器一般不还页，``peak == end``
    既可能是没逐出、也可能是逐出了。
    """
    payloads = [{"session_id": session_id} for session_id in session_ids]
    results = await _post_many(env, "/api/session/release", payloads)
    reaped = 0
    still_loaded = 0
    problems: list[str] = []
    for index, result in enumerate(results):
        if isinstance(result, str):
            problems.append(result)
            continue
        if result.status_code != 200:
            problems.append(
                f"{payloads[index]['session_id']}: {result.status_code} "
                f"{result.text[:120]}"
            )
            continue
        if result.json().get("released"):
            still_loaded += 1
        else:
            reaped += 1
    if problems:
        notes.append(
            f"eviction is not observable here ({len(problems)} of "
            f"{len(session_ids)} release calls failed; first: {problems[0]}) — "
            "evicted_by_reaper / released_by_probe are null, not 0"
        )
        return
    record["samples"]["evicted_by_reaper"] = reaped
    record["samples"]["released_by_probe"] = still_loaded
    notes.append(
        f"eviction probe: {reaped} session(s) were already out of memory when the "
        f"window ended (reaped), {still_loaded} were still loaded and got released by "
        "the probe"
    )


async def _spawn_shards(
    cfg: RunConfig,
    session_ids: Sequence[str],
    env: ProbeEnv,
    level_dir: Path,
    notes: list[str],
    handles: list[_ShardHandle],
    plan: shapes.ShapePlan,
) -> None:
    """轮转切分会话并 spawn 分片子进程（配置 / 样本 / 日志各一份文件）。

    ``handles`` 由调用方持有：spawn 中途失败时已起的分片仍在它的 ``finally`` 里，
    不会留下孤儿进程。
    """
    count = cfg.shards_for(len(session_ids))
    if count != cfg.shards:
        notes.append(
            f"shards clamped from {cfg.shards} to {count} (sessions={len(session_ids)})"
        )
    for index in range(count):
        handles.append(
            await _spawn_shard(
                index=index,
                sessions=tuple(session_ids[index::count]),
                mode=plan.window_mode,
                seed_turns=0,
                env=env,
                level_dir=level_dir,
                subscribers=cfg.subscribers,
                shape=cfg.scenario,
            )
        )
    return None


async def _spawn_shard(
    *,
    index: int,
    sessions: tuple[str, ...],
    mode: str,
    seed_turns: int,
    env: ProbeEnv,
    level_dir: Path,
    subscribers: str,
    shape: str,
) -> _ShardHandle:
    """写分片配置并 spawn 一个分片子进程。"""
    level_dir.mkdir(parents=True, exist_ok=True)
    config_path = level_dir / f"shard-{index}.json"
    out_path = level_dir / f"shard-{index}.samples.json"
    log_path = level_dir / f"shard-{index}.log"
    config_path.write_text(
        json.dumps(
            {
                "shard": index,
                "gateway_url": env.gateway_url,
                "sessions": list(sessions),
                "subscribers": subscribers,
                "shape": shape,
                "out": str(out_path),
                "mode": mode,
                "seed_turns": seed_turns,
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )
    log_handle = open(log_path, "wb")
    try:
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
    except OSError:
        log_handle.close()
        raise
    return _ShardHandle(
        index=index,
        process=process,
        config_path=config_path,
        out_path=out_path,
        log_path=log_path,
        log_handle=log_handle,
    )


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
        raise HandshakeTimeout(
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


async def _sample_window(env: ProbeEnv, sampling: _Sampling, interval: float) -> None:
    """窗口内采样：CPU 首尾累计差 + RSS/fd 周期峰值。

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
        await asyncio.sleep(min(interval, remaining))
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


async def _collect(
    handles: list[_ShardHandle],
) -> tuple[list[dict[str, Any]], list[str]]:
    """等每个分片报 done / 退出，读回样本文件；**逐分片容错**。

    崩掉一个分片不该让同一档里其余分片已经采到的样本变成 ``null``（那一档的网关数字
    依然是真读数，崩机现场恰恰是最有价值的一格）。失败的分片只贡献一条问题记录。
    """
    payloads: list[dict[str, Any]] = []
    problems: list[str] = []
    for handle in handles:
        try:
            try:
                await _read_event(handle, expect="done", timeout=SHARD_EXIT_TIMEOUT)
                code = await asyncio.wait_for(handle.process.wait(), SHARD_EXIT_TIMEOUT)
                if code != 0:
                    raise OrchestratorError(
                        f"shard {handle.index} exited with code {code}\n"
                        f"{read_log_tail(handle.log_path)}"
                    )
                payload = _read_samples(handle)
            except HandshakeTimeout:
                # 握手落空 ≠ 没测到：分片先写样本再报 done，正忙时握手会被积压拖住。
                payload = await _await_samples(handle)
            metrics.check_shard_payload(payload)
        except Exception as exc:  # noqa: BLE001 - 单分片失败只记账，不丢其余样本
            problems.append(f"shard {handle.index}: {type(exc).__name__}: {exc}")
            continue
        payloads.append(payload)
    return payloads, problems


async def _await_samples(handle: _ShardHandle) -> dict[str, Any]:
    """握手落空后等样本文件落全：分片一次性写出整份，能解析成 JSON 就是写完了。"""
    deadline = time.monotonic() + SHARD_DRAIN_TIMEOUT
    while True:
        try:
            return _read_samples(handle)
        except OrchestratorError as exc:
            last = exc
        if time.monotonic() >= deadline:
            raise OrchestratorError(
                f"shard {handle.index} wrote no usable samples within "
                f"{SHARD_DRAIN_TIMEOUT:.0f}s of missing its 'done' handshake "
                f"(code {handle.process.returncode}): {last}\n"
                f"{read_log_tail(handle.log_path)}"
            ) from last
        await asyncio.sleep(0.5)


def _read_samples(handle: _ShardHandle) -> dict[str, Any]:
    """读样本文件（半截 JSON / 非对象都算该分片失败，不污染其余分片）。"""
    try:
        payload = json.loads(handle.out_path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise OrchestratorError(
            f"shard {handle.index} wrote no sample file ({handle.out_path})"
        ) from exc
    except (OSError, ValueError) as exc:
        raise OrchestratorError(
            f"shard {handle.index} samples are unreadable: {type(exc).__name__}: {exc}"
        ) from exc
    if not isinstance(payload, dict):
        raise OrchestratorError(
            f"shard {handle.index} sample file is not a JSON object"
        )
    return payload


# ── 指标装配 ──────────────────────────────────────────────────


def _turns_from_payloads(
    payloads: Sequence[dict[str, Any]], window_start: float
) -> list[TurnSample]:
    """分片样本 → 客户端侧事实（时间戳从「相对窗口的微秒」还原成绝对单调时刻）。"""
    return [
        TurnSample(
            prompt=str(entry["prompt"]),
            sent_at=window_start + int(entry["sent_us"]) / 1e6,
            result_at=None
            if entry.get("result_us") is None
            else window_start + int(entry["result_us"]) / 1e6,
            arrivals={
                kind: tuple(window_start + int(value) / 1e6 for value in values)
                for kind, values in (entry.get("arrivals_us") or {}).items()
            },
            hydrate_sent_at=None
            if entry.get("hydrate_sent_us") is None
            else window_start + int(entry["hydrate_sent_us"]) / 1e6,
            hydrated_at=None
            if entry.get("sync_us") is None
            else window_start + int(entry["sync_us"]) / 1e6,
        )
        for payload in payloads
        for entry in payload["turns"]
    ]


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
    turns = _turns_from_payloads(payloads, window_start)
    delta_frames = {
        kind: sum(
            int((payload["delta_frames"] or {}).get(kind, 0)) for payload in payloads
        )
        for kind in metrics.CHANNEL_EVENTS
    }
    content_frames = delta_frames["content"]
    all_events = sum(delta_frames.values())
    window_chars = sum(int(payload["chars"]) for payload in payloads)
    errors = sum(int(payload["errors"]) for payload in payloads)
    completed = [turn for turn in turns if turn.result_at is not None]
    wall = sampling.wall_s
    block = record["metrics"]
    samples = record["samples"]
    if payloads:
        samples["delta_frames"] = delta_frames
        samples["text_frames"] = content_frames
        samples["chars"] = window_chars
        samples["turns"] = len(completed)
        samples["turns_in_flight"] = len(turns) - len(completed)
        samples["sync_sessions"] = sum(
            int(payload["sync_sessions"]) for payload in payloads
        )
        samples["hydration_attempts"] = sum(
            int(payload["hydration_attempts"]) for payload in payloads
        )
        samples["hydration_failed"] = sum(
            int(payload["hydration_failed"]) for payload in payloads
        )
    record["wall_s"] = round(wall, 3) if wall > 0 else None

    block["gateway.cpu_cores"] = _ratio(sampling.gateway_cpu_seconds, wall)
    if sampling.rss_mb:
        block["gateway.rss_mb_peak"] = round(max(sampling.rss_mb), 1)
        block["gateway.rss_mb_end"] = round(sampling.rss_mb[-1], 1)
    if sampling.fds:
        block["gateway.fds_peak"] = max(sampling.fds)
    block["slow_client_recycles"] = (
        sum(int(payload["closed_before_end"]) for payload in payloads)
        if payloads
        else None
    )

    if payloads:
        block["errors"] = errors
        if wall > 0:
            if content_frames:
                block["tokens_per_second_actual"] = round(content_frames / wall, 1)
            if all_events:
                block["events_per_second_actual"] = round(all_events / wall, 1)
                block["chars_per_second"] = round(window_chars / wall, 1)
            # 完成的轮次数是可以真实为 0 的计数（单轮形态窗口里没有轮次边界）。
            block["turns_per_second"] = round(len(completed) / wall, 3)
        if not all_events:
            notes.append(
                "tokens_per_second_actual / chars_per_second are null: no delta event "
                "arrived in the window"
                if wall > 0
                else "tokens_per_second_actual / chars_per_second are null: the sample "
                "window did not complete"
            )
    turn_spread = metrics.spread(
        [(turn.result_at - turn.sent_at) * 1000.0 for turn in completed]
    )
    block["turn.p50_ms"] = _round(turn_spread.get("p50"))
    block["turn.p99_ms"] = _round(turn_spread.get("p99"))
    if payloads and not completed:
        notes.append(_no_turn_note(record["shape"].get("window_mode"), cfg.scenario))
    _fill_hydration(block, samples, turns, notes)

    delivery_available = provider is not None and metrics.has_frame_emits(provider)
    samples["delivery_available"] = None if provider is None else delivery_available
    if delivery_available and turns:
        pairing = metrics.pair_delivery(
            metrics.build_emit_ledger(provider),
            provider.requests.all(),
            turns,
            window=(sampling.window_start, sampling.window_end),
        )
        for label, value in metrics.spread(pairing.latencies_ms).items():
            block[f"delivery.{label}_ms"] = _round(value)
        samples["count"] = pairing.paired
        samples["dropped"] = pairing.dropped
        samples["dropped_reasons"] = dict(pairing.reasons)
        samples["paired_channels"] = dict(pairing.channels)
        channels = ", ".join(
            f"{kind}={count}" for kind, count in sorted(pairing.channels.items())
        )
        notes.append(
            "delivery pairing is per channel ("
            + ", ".join(
                f"{kind}→{event}" for kind, event in metrics.CHANNEL_EVENTS.items()
            )
            + "), meta frames excluded; paired "
            + (channels or "nothing")
        )
        if pairing.dropped:
            notes.append(
                f"delivery pairing dropped {pairing.dropped} frame(s): "
                f"{dict(pairing.reasons)}"
            )
    elif provider is not None and not delivery_available:
        notes.append(DELIVERY_UNAVAILABLE)
    elif delivery_available:
        notes.append("delivery.* is null: no turn sample was collected in the window")

    shard_cpu: list[float | None] = []
    for payload in payloads:
        shard_cpu.append(_shard_cpu_cores(payload, wall, notes))
        notes.extend((payload.get("error_details") or ())[:2])
    if set(block) != set(METRIC_KEYS):
        raise OrchestratorError(
            f"metrics keys drifted from METRIC_KEYS: "
            f"{sorted(set(block) ^ set(METRIC_KEYS))}"
        )
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
    if record["shape"]["shards"] is None:
        record["shape"]["shards"] = len(payloads) or None
    return record


def _no_turn_note(window_mode: str | None, turn_boundaries: bool | None) -> str:
    """「窗口里没有完成的轮次」的原因按形态声明说（结构性 null ≠ 没测到）。"""
    if window_mode == "hold":
        return (
            "turn.* is null: this shape drives nothing, so no turn can complete inside "
            "the window"
        )
    if turn_boundaries:
        return (
            "turn.* is null: no turn completed inside the window — the cycle is longer "
            "than the window (--cycle-s), the cycling group was empty at this level, or "
            "the gateway stalled"
        )
    return (
        "turn.* is null: no turn completed inside the window — the streaming shape "
        "streams one turn per session across warmup + dwell by design (per-turn fixed "
        "cost is measured by the tool-cycle shape)"
    )


def _fill_hydration(
    block: dict[str, Any],
    samples: dict[str, Any],
    turns: Sequence[TurnSample],
    notes: list[str],
) -> None:
    """水合与首帧：两段**不同的**时间线，各自成指标（不混成一个「启动耗时」）。

    ``hydration.*`` = 订阅请求发出 → ``sync_session`` 到达（读 history + 拼快照 + 过 wire）；
    ``first_frame.*`` = 驱动 prompt 发出 → 首个正文增量。只有水合形态会有它们；
    其余形态两者都留 ``null``（结构上没有订阅发生在窗口里）。
    """
    hydrated = [
        (turn.hydrated_at - turn.hydrate_sent_at) * 1000.0
        for turn in turns
        if turn.hydrated_at is not None and turn.hydrate_sent_at is not None
    ]
    first_frames = [
        (turn.arrivals["content"][0] - turn.sent_at) * 1000.0
        for turn in turns
        if turn.arrivals.get("content")
        and turn.hydrated_at is not None
        and turn.arrivals["content"][0] >= turn.sent_at
    ]
    for label, value in metrics.spread(hydrated).items():
        block[f"hydration.{label}_ms"] = _round(value)
    for label, value in metrics.spread(first_frames).items():
        block[f"first_frame.{label}_ms"] = _round(value)
    if not hydrated and samples.get("hydration_attempts"):
        notes.append("hydration.* is null: no subscription completed inside the window")


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


def _first_line(text: str) -> str:
    """多行诊断的第一行（完整文本在 ``error_detail`` / 日志里）。"""
    return text.splitlines()[0] if text.splitlines() else text


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
