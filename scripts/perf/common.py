#!/usr/bin/env python3
"""perf harness 的共用工具层：side 描述符（契约 §1）、suite 输出（§3）、
阈值与判定（§4/§8）、JSON 与子进程。

`suite_<name>.py`（04/05/06 的测量本体）与 `ab.py`（编排）都 import 本模块，
所以这里的 API 是三个 suite 步骤的共同依赖面：契约见 perf-ci 任务书
`§Frozen Interfaces`，拆分理由见 01 harness_core 步骤的 `design.md`（D1）。

只用标准库。
"""

from __future__ import annotations

import fnmatch
import json
import math
import os
import shlex
import subprocess
import time
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

#: side 描述符（契约 §1）的必填字段。
SIDE_FIELDS: tuple[str, ...] = (
    "name",
    "worktree",
    "wing_bin",
    "gateway_bin",
    "python",
    "wing_home",
)

#: metric id 的单位后缀（契约 §4）；报告按它选显示单位。
UNIT_SUFFIXES: tuple[str, ...] = ("_ns", "_us", "_ms", "_ratio", "_pct")

#: 单位后缀 → 显示单位（`_ratio` 无量纲）。
_UNIT_DISPLAY: Mapping[str, str] = {
    "_ns": "ns",
    "_us": "µs",
    "_ms": "ms",
    "_ratio": "",
    "_pct": "%",
}

#: 日志尾部带给错误信息的字符数上限。
LOG_TAIL_CHARS = 1200


class CommandError(RuntimeError):
    """子进程无法执行（可执行文件缺失等）——错误信息要能直接照做。"""


# ── JSON ──────────────────────────────────────────────────────


def read_json(path: Path) -> Any:
    """读 JSON；错误信息带路径，方便在 CI 日志里定位。"""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise CommandError(f"cannot read {path}: {exc}") from exc
    try:
        return json.loads(text)
    except json.JSONDecodeError as exc:
        raise ValueError(f"{path}: invalid JSON: {exc}") from exc


def write_json(path: Path, payload: Any) -> None:
    """原子写 JSON（tmp + rename），并拒绝 NaN/Infinity（非标准 JSON）。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(payload, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


# ── 数值 ──────────────────────────────────────────────────────


def median(values: Sequence[float]) -> float:
    """中位数；偶数个取中间两个的均值。空序列 → ValueError。"""
    ordered = sorted(values)
    if not ordered:
        raise ValueError("median() of an empty sequence")
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return float(ordered[mid])
    return float((ordered[mid - 1] + ordered[mid]) / 2.0)


def finite(values: Iterable[Any]) -> list[float]:
    """转成 float 并丢掉非有限 / 非数值条目（NaN、Infinity、布尔、字符串）。

    坏样本不参与中位数——静默当 0 会把噪声写成结论。
    """
    out: list[float] = []
    for value in values:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            continue
        number = float(value)
        if math.isfinite(number):
            out.append(number)
    return out


def metric_unit(metric_id: str) -> str:
    """metric id → 显示单位（未知后缀返回空串，不猜）。"""
    for suffix in UNIT_SUFFIXES:
        if metric_id.endswith(suffix):
            return _UNIT_DISPLAY[suffix]
    return ""


# ── side 描述符（契约 §1） ─────────────────────────────────────


@dataclass(frozen=True)
class Side:
    """一侧（base / head）的启动描述：同一 launch 机制，只允许代码来源不同。"""

    name: str
    worktree: Path
    wing_bin: Path
    gateway_bin: Path
    python: Path
    wing_home: Path

    @classmethod
    def load(cls, path: Path) -> Side:
        payload = read_json(path)
        if not isinstance(payload, Mapping):
            raise ValueError(f"{path}: side descriptor must be a JSON object")
        missing = [key for key in SIDE_FIELDS if key not in payload]
        if missing:
            raise ValueError(f"{path}: side descriptor missing {', '.join(missing)}")
        return cls(
            name=str(payload["name"]),
            worktree=Path(str(payload["worktree"])),
            wing_bin=Path(str(payload["wing_bin"])),
            gateway_bin=Path(str(payload["gateway_bin"])),
            python=Path(str(payload["python"])),
            wing_home=Path(str(payload["wing_home"])),
        )

    def to_json(self) -> dict[str, str]:
        return {key: str(getattr(self, key)) for key in SIDE_FIELDS}

    def write(self, path: Path) -> None:
        write_json(path, self.to_json())


# ── suite 输出（契约 §3） ──────────────────────────────────────


@dataclass
class SuiteOutput:
    """suite 侧的输出构造器：`suite_<name>.py` 用它落盘契约 §3 的 JSON。"""

    suite: str
    side: str
    round: int
    quick: bool = False
    started: float = field(default_factory=time.monotonic)
    metrics: dict[str, float] = field(default_factory=dict)
    samples: dict[str, list[float]] = field(default_factory=dict)
    notes: list[str] = field(default_factory=list)
    config: dict[str, Any] = field(default_factory=dict)

    def metric(self, metric_id: str, value: float, *samples: float) -> None:
        """记一条本轮的代表值（通常是样本中位数）；`samples` 可选，只作证据。"""
        self.metrics[metric_id] = float(value)
        if samples:
            self.samples.setdefault(metric_id, []).extend(float(x) for x in samples)

    def sample(self, metric_id: str, value: float) -> None:
        """只记原始样本：ab.py 会退化成"样本中位数 = 本轮代表值"。"""
        self.samples.setdefault(metric_id, []).append(float(value))

    def note(self, text: str) -> None:
        self.notes.append(str(text))

    def setting(self, key: str, value: Any) -> None:
        """进 `meta.config`：置信信息 / 配置（契约 §2 要求低置信也写进 meta）。"""
        self.config[key] = value

    def payload(self, *, ok: bool = True, error: str | None = None) -> dict[str, Any]:
        config = dict(self.config)
        config.setdefault("quick", self.quick)
        return {
            "suite": self.suite,
            "side": self.side,
            "round": self.round,
            "ok": ok,
            "error": error,
            "metrics": dict(self.metrics),
            "samples": {key: list(values) for key, values in self.samples.items()},
            "meta": {
                "duration_s": round(time.monotonic() - self.started, 3),
                "notes": list(self.notes),
                "config": config,
            },
        }

    def write(self, path: Path, *, ok: bool = True, error: str | None = None) -> None:
        write_json(path, self.payload(ok=ok, error=error))


@dataclass(frozen=True)
class SuiteResult:
    """ab 侧解析 / 校验过的 suite 输出。"""

    suite: str
    side: str
    round: int
    ok: bool
    error: str | None
    metrics: Mapping[str, float]
    samples: Mapping[str, Sequence[float]]
    meta: Mapping[str, Any]

    @classmethod
    def parse(cls, payload: Any, path: Path) -> SuiteResult:
        if not isinstance(payload, Mapping):
            raise ValueError(f"{path}: suite output must be a JSON object")
        missing = [
            key
            for key in ("suite", "side", "round", "ok", "metrics", "samples")
            if key not in payload
        ]
        if missing:
            raise ValueError(f"{path}: suite output missing {', '.join(missing)}")
        metrics = payload["metrics"]
        samples = payload["samples"]
        if not isinstance(metrics, Mapping) or not isinstance(samples, Mapping):
            raise ValueError(f"{path}: 'metrics' and 'samples' must be JSON objects")
        meta = payload.get("meta")
        return cls(
            suite=str(payload["suite"]),
            side=str(payload["side"]),
            round=int(payload["round"]),
            ok=bool(payload["ok"]),
            error=None if payload.get("error") is None else str(payload["error"]),
            metrics=metrics,
            samples=samples,
            meta=meta if isinstance(meta, Mapping) else {},
        )

    @property
    def notes(self) -> tuple[str, ...]:
        raw = self.meta.get("notes")
        if not isinstance(raw, Sequence) or isinstance(raw, (str, bytes)):
            return ()
        return tuple(str(item) for item in raw)

    def round_value(self, metric_id: str) -> float | None:
        """本轮代表值：`metrics` 优先；缺失 / 非有限则退化为样本中位数。"""
        direct = finite([self.metrics.get(metric_id)])
        if direct:
            return direct[0]
        return _median_or_none(finite(self.samples.get(metric_id, ())))


def _median_or_none(values: Sequence[float]) -> float | None:
    return median(values) if values else None


# ── 失败条目（ab 输出 JSON 的 failures[]） ─────────────────────


@dataclass(frozen=True)
class Failure:
    """一次可归因的失败：suite 非 0 退出、prepare 失败、输出不合契约等。"""

    suite: str
    side: str | None
    round: int | None
    error: str
    exit_code: int | None = None
    log: str | None = None
    log_tail: str | None = None

    def to_json(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "suite": self.suite,
            "side": self.side,
            "round": self.round,
            "error": self.error,
        }
        if self.exit_code is not None:
            payload["exit_code"] = self.exit_code
        if self.log is not None:
            payload["log"] = self.log
        if self.log_tail is not None:
            payload["log_tail"] = self.log_tail
        return payload


# ── 阈值与判定（契约 §4/§8） ───────────────────────────────────


@dataclass(frozen=True)
class Thresholds:
    """噪声带 / 回归线 / 方向约定；`overrides` 键可以是 suite 名或 metric 模式。"""

    noise_pct: float = 6.0
    regress_pct: float = 20.0
    overrides: Mapping[str, Mapping[str, float]] = field(default_factory=dict)
    higher_better: tuple[str, ...] = ()

    @classmethod
    def load(cls, path: Path) -> Thresholds:
        payload = read_json(path)
        if not isinstance(payload, Mapping):
            raise ValueError(f"{path}: thresholds must be a JSON object")
        default = payload.get("default") or {}
        if not isinstance(default, Mapping):
            raise ValueError(f"{path}: 'default' must be a JSON object")
        raw_overrides = payload.get("overrides") or {}
        if not isinstance(raw_overrides, Mapping):
            raise ValueError(f"{path}: 'overrides' must be a JSON object")
        overrides: dict[str, dict[str, float]] = {}
        for key, value in raw_overrides.items():
            if not isinstance(value, Mapping):
                raise ValueError(f"{path}: override {key!r} must be a JSON object")
            unknown = [k for k in value if k not in ("noise_pct", "regress_pct")]
            if unknown:
                raise ValueError(
                    f"{path}: override {key!r} has unknown keys {', '.join(map(str, unknown))}"
                )
            overrides[str(key)] = {
                str(k): float(v)
                for k, v in value.items()
                if k in ("noise_pct", "regress_pct")
            }
        higher_better = payload.get("higher_better") or []
        if not isinstance(higher_better, Sequence) or isinstance(
            higher_better, (str, bytes)
        ):
            raise ValueError(
                f"{path}: 'higher_better' must be a list of metric patterns"
            )
        return cls(
            noise_pct=float(default.get("noise_pct", cls.noise_pct)),
            regress_pct=float(default.get("regress_pct", cls.regress_pct)),
            overrides=overrides,
            higher_better=tuple(str(item) for item in higher_better),
        )

    def for_metric(self, metric: str, suite: str) -> tuple[float, float]:
        """返回该 metric 的 (noise_pct, regress_pct)；metric 模式优先于 suite 名。"""
        for key, override in self.overrides.items():
            if fnmatch.fnmatchcase(metric, key):
                return (
                    float(override.get("noise_pct", self.noise_pct)),
                    float(override.get("regress_pct", self.regress_pct)),
                )
        suite_override = self.overrides.get(suite)
        if suite_override is not None:
            return (
                float(suite_override.get("noise_pct", self.noise_pct)),
                float(suite_override.get("regress_pct", self.regress_pct)),
            )
        return (self.noise_pct, self.regress_pct)

    def is_higher_better(self, metric: str) -> bool:
        return any(
            fnmatch.fnmatchcase(metric, pattern) for pattern in self.higher_better
        )


def verdict_for(
    delta_pct: float, *, noise_pct: float, regress_pct: float, higher_better: bool
) -> str:
    """契约 §5 的四档判定：flat / improved / watch / regression（|Δ| = regress 算回归）。"""
    magnitude = abs(delta_pct)
    if delta_pct == 0 or magnitude < noise_pct:
        return "flat"
    better = delta_pct > 0 if higher_better else delta_pct < 0
    if better:
        return "improved"
    return "regression" if magnitude >= regress_pct else "watch"


# ── 子进程 ────────────────────────────────────────────────────


@dataclass(frozen=True)
class CommandResult:
    rc: int
    seconds: float
    log: Path | None
    tail: str


def run_command(
    cmd: Sequence[str],
    *,
    cwd: Path | None = None,
    log_path: Path | None = None,
    env: Mapping[str, str] | None = None,
    timeout: float | None = None,
    echo: bool = False,
) -> CommandResult:
    """跑一条命令，输出合并到 `log_path`（无则收进内存），返回退出码与日志尾部。"""
    display = " ".join(shlex.quote(str(part)) for part in cmd)
    if echo:
        print(f"[perf] $ {display}", flush=True)
    started = time.monotonic()
    handle = None
    captured = ""
    try:
        if log_path is not None:
            log_path.parent.mkdir(parents=True, exist_ok=True)
            handle = log_path.open("w", encoding="utf-8", errors="replace")
            handle.write(f"$ {display}\n")
            handle.flush()
        try:
            proc = subprocess.run(
                [str(part) for part in cmd],
                cwd=None if cwd is None else str(cwd),
                env=None if env is None else dict(env),
                stdout=handle if handle is not None else subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=timeout,
                check=False,
            )
            rc = proc.returncode
            captured = proc.stdout or ""
        except subprocess.TimeoutExpired:
            # subprocess.run 超时会杀掉子进程；这里只把它记成明确的非 0。
            rc = 124
            if handle is not None:
                handle.write(f"\n[perf] timeout after {timeout}s\n")
    except OSError as exc:
        raise CommandError(f"cannot execute {cmd[0]!r}: {exc}") from exc
    finally:
        if handle is not None:
            handle.close()
    tail = captured[-LOG_TAIL_CHARS:]
    if log_path is not None:
        tail = _tail(log_path)
    return CommandResult(
        rc=rc, seconds=round(time.monotonic() - started, 3), log=log_path, tail=tail
    )


def _tail(path: Path, max_chars: int = LOG_TAIL_CHARS) -> str:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""
    return text[-max_chars:].strip()
