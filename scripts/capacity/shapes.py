#!/usr/bin/env python3
"""负载形态注册表：``--scenario`` 的唯一取值来源，也是形态契约的声明处。

``run.py``（注册剧本 / 校验参数）与 ``driver_shard.py``（发 prompt）都经
:func:`resolve` 拿同一个模块对象，因此形态的「剧本」与「prompt 文本」只有一处定义
——否则配对键会在两侧漂移，延迟样本会静默变成全 ``dropped``。

形态契约（每个形态模块都要满足，:func:`resolve` 校验前三项）：

- ``NAME``：``--scenario`` 的取值；
- ``MODEL``：形态的基础 model id（``providers[].models`` 里的 id）；
- ``plan(args) -> ShapePlan``：本档要注册的 model / 剧本、每会话挂哪个 model、
  工具集、workspace 里的预置文件、窗口模式……形态的全部语义都在这一个返回值里
  （见 :class:`ShapePlan`）。

可选声明（缺省即「这个形态不用它」）：

- ``KNOBS``：形态消费的可选旋钮（``tool`` / ``cycle_s`` / ``decode_duty`` /
  ``history_tokens``）；显式给非默认值的旋钮而形态不消费它 → 报错，不静默忽略；
- ``prompt_text(session_id, seq) -> str``：驱动轮的**全局唯一** prompt 文本（配对键）。
  缺省即 :func:`prompt_key`（所有形态目前都用它），形态需要别的前缀时才声明。

第三种「形态没有自己的剧本」的情况不存在：``idle`` 不驱动也不写剧本，但仍要给假
Provider 一个合法的 model（会话创建时要求 model 落在 id 空间里）。
"""

from __future__ import annotations

import importlib
import math
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field, replace
from types import ModuleType
from typing import Any

from wing.common.token_counter import TokenCounter
from wing_probe.provider.script import Script, Turn

SHAPES: dict[str, str] = {
    "pure_stream": "shape_pure_stream",
    "tool_cycle": "shape_tool_cycle",
    "idle": "shape_idle",
    "hydration": "shape_hydration",
    "mixed": "shape_mixed",
}
"""``--scenario`` → 模块名（本目录内的同级模块）。"""

#: 可选旋钮的默认值（CLI 的 argparse 默认值也取自这里，避免两处数字）。
DEFAULT_CHUNK = 1
DEFAULT_TOOL = "read"
DEFAULT_CYCLE_S = 3.0
DEFAULT_DECODE_DUTY = 0.7

#: 形态可以声明的可选旋钮（见模块 docstring）。
OPTIONAL_KNOBS: tuple[str, ...] = ("tool", "cycle_s", "decode_duty", "history_tokens")

#: 分片在窗口内做什么（见 ``driver_shard``）。
WINDOW_MODES: tuple[str, ...] = ("drive", "hold", "hydrate")

#: 预置历史用的 model：与形态自己的 model 分开，预置轮与测量轮各有独立游标。
SEED_MODEL = "capacity/seed"

#: 预置历史的每帧字符数（大文本一轮吐出，帧数太少会让「分帧」变成单帧）。
SEED_FRAME_CHARS = 4000

#: 每 token 对应的字符数：产品估算器的非 CJK 比例是**唯一**事实来源
#: （``TokenCounter.NON_CJK_RATIO``）——`--history-tokens` 的语义完全依赖它，
#: 抄一份字面量会让产品调整估算后这里的"token 数"静默变味。
HISTORY_CHARS_PER_TOKEN = 1 / TokenCounter.NON_CJK_RATIO

_REQUIRED = ("NAME", "MODEL", "plan")


class ShapeError(ValueError):
    """未知形态、形态模块没满足契约、或旋钮与形态不匹配。"""


def names() -> list[str]:
    return sorted(SHAPES)


def resolve(name: str) -> ModuleType:
    """按名字取形态模块（未知名 / 契约不全 → ``ShapeError``，附可选项）。"""
    module_name = SHAPES.get(name)
    if module_name is None:
        raise ShapeError(
            f"unknown scenario {name!r}; available: {', '.join(names()) or '<none>'}"
        )
    module = importlib.import_module(module_name)
    missing = [attr for attr in _REQUIRED if not hasattr(module, attr)]
    if missing:
        raise ShapeError(
            f"scenario module {module_name!r} does not implement the shape "
            f"contract: missing {', '.join(missing)}"
        )
    if module.NAME != name:
        raise ShapeError(
            f"scenario {name!r} is registered to module {module_name!r}, but the "
            f"module declares NAME={module.NAME!r}"
        )
    return module


# ── prompt 文本（配对键的唯一构造处）────────────────────────────


def prompt_key(session_id: str, seq: int) -> str:
    """驱动轮的用户正文（全局唯一：配对靠它在 ``RequestLog`` 里定位请求）。"""
    return f"load:{session_id}:{seq}"


def seed_key(session_id: str, seq: int) -> str:
    """预置历史轮的用户正文（前缀不同 ⇒ 永远不会与驱动轮撞配对键）。"""
    return f"seed:{session_id}:{seq}"


def prompt_text_of(shape: ModuleType) -> Callable[[str, int], str]:
    """形态的驱动轮 prompt 构造器（未声明即 :func:`prompt_key`）。"""
    return getattr(shape, "prompt_text", prompt_key)


# ── 形态输入 ───────────────────────────────────────────────────


@dataclass(frozen=True)
class ShapeArgs:
    """一档的形态参数（``orchestrator`` 从 ``RunConfig`` + 档位信息组装）。"""

    scenario: str
    sessions: int
    rate: float
    """每会话**事件**/s（一个事件 = 上游 SSE 一个 delta 帧 = 网关一次 emit）。"""
    span_s: float
    """档位时长（warmup + dwell）。"""
    dwell_s: float
    """采样窗口长度（``--dwell``）：逐出阈值之类"必须在窗口内发生"的量按它推。"""
    chunk: int
    """每个事件的字符数（``--chunk``）。"""
    cycle_s: float
    decode_duty: float
    tool: str
    history_tokens: int
    frame_delay_s: float | None = None
    """协调者实测后的帧间隔（``1/rate`` 减去本机 ``asyncio.sleep`` 超时）。"""

    @property
    def frame_delay(self) -> float:
        return 1.0 / self.rate if self.frame_delay_s is None else self.frame_delay_s

    @property
    def warmup_s(self) -> float:
        """窗口之前的预热时长（``--warmup``）。

        需要它的量只有一类：空闲计时之类的窗口内事件阈值——会话的空闲计时从建会话起算，
        所以阈值必须相对 warmup 偏移。
        """
        return self.span_s - self.dwell_s

    def events_for(self, seconds: float) -> int:
        """覆盖 ``seconds`` 秒所需的事件数（至少 1）。"""
        return max(1, math.ceil(self.rate * seconds))

    def chars_for(self, seconds: float) -> int:
        """覆盖 ``seconds`` 秒所需的字符数（= 事件数 × chunk）。"""
        return self.events_for(seconds) * self.chunk


@dataclass(frozen=True)
class SeedSpec:
    """历史预置（``--history-tokens`` > 0 时由 :func:`build_plan` 挂上）。"""

    model: str
    script: Script
    tokens_per_session: int
    chars_per_session: int


@dataclass(frozen=True)
class ShapePlan:
    """一档的形态决定（``orchestrator`` 只按这个返回值装配，不再看形态模块）。"""

    models: tuple[tuple[str, Script], ...]
    """本档要声明并注册的 ``(model_id, script)``。"""
    session_models: tuple[str, ...]
    """第 i 个会话挂哪个 model id（长度必须等于会话数）。"""
    tools: tuple[str, ...]
    """模板的工具集（工具 schema 会摊进每轮请求，所以只要用到的那几个）。"""
    drives_upstream: bool
    """窗口里每个会话是否占住一条上游流。

    ``False`` 只有「不驱动任何请求」的形态（``idle``）：它们不建上游连接，因此连接池
    上限与它们的读数无关。判据据此决定「``sessions > 池上限``」适不适用。

    必填而非默认：忘记声明就装不进 ``ShapePlan``，不会静默套上别人的事实。
    """
    turn_boundaries: bool
    """窗口里是否存在多个轮次边界（每轮都会收口）⇒ ``turn.p99`` 可评。

    ``False`` = 单轮 / 不驱动：整窗只有一条流（``pure_stream``）或一个水合轮
    （``hydration``），没有「每轮固定成本」可比。
    """
    agent: Mapping[str, Any] = field(default_factory=dict)
    """会话 create 期的 agent 覆盖（例如 bash 变体的 ``yolo``）。"""
    workspace_files: Mapping[str, str] = field(default_factory=dict)
    """会话 workspace 里要预置的文件（``Read`` 参数指向它们）。"""
    subscribe: bool = True
    """分片是否在窗口前订阅（订阅会钉住会话，逐出形态必须关掉）。"""
    window_mode: str = "drive"
    sample_interval_s: float = 1.0
    """RSS / fd 的采样间隔（尖峰形态要更密）。"""
    env_sessions: Mapping[str, Any] | None = None
    """probe 环境的 ``sessions:`` 配置段（逐出形态把阈值压进窗口）。"""
    measure_eviction: bool = False
    """窗口结束后是否用 ``/api/session/release`` 确认逐出发生过。"""
    seed: SeedSpec | None = None


# ── 剧本原语 ───────────────────────────────────────────────────


class CyclicScript(Script):
    """按固定模式**无限循环**的剧本。

    工具循环形态没有可写出的轮数上界：``--cycle-s 0`` 时轮次频率只受机器限制，预生成
    一个「够用」的有限剧本必然在某档上用尽（剧本耗尽 = 假 Provider 5xx = 假错误）。
    循环模式（两轮一组）因此是唯一自洽的表示；``consumed`` 仍是真实的服务轮数。
    """

    def __init__(self, *turns: Turn) -> None:
        super().__init__(*turns)
        self._pattern = tuple(turns)

    def consume(self, *, model: str = "") -> tuple[Turn, int]:
        index = self._consumed
        self._consumed += 1
        return self._pattern[index % len(self._pattern)], index


def covering_turn(args: ShapeArgs, seconds: float, **kwargs: Any) -> Turn:
    """覆盖 ``seconds`` 秒的一轮（帧大小 = ``chunk``，帧间隔由 pacer 决定）。

    余量（``TURN_SLACK_SECONDS``）由调用方加在 ``seconds`` 上。
    """
    return Turn.of(
        text="x" * args.chars_for(seconds),
        chunk=args.chunk,
        delay=args.frame_delay,
        **kwargs,
    )


# ── 装配与校验 ─────────────────────────────────────────────────


def build_plan(args: ShapeArgs) -> ShapePlan:
    """跑形态的 ``plan()``，校验契约与旋钮适用范围，必要时挂上历史预置。"""
    shape = resolve(args.scenario)
    knobs = frozenset(getattr(shape, "KNOBS", ()))
    _check_knobs(shape, knobs, args)
    plan = shape.plan(args)
    _validate(shape, plan, args)
    if args.history_tokens > 0:
        plan = _with_seed(plan, args)
    return plan


def _check_knobs(shape: ModuleType, knobs: frozenset[str], args: ShapeArgs) -> None:
    """形态不消费的旋钮不许显式给非默认值（静默忽略会让人以为测了那个变量）。"""
    given = {
        "tool": args.tool != DEFAULT_TOOL,
        "cycle_s": args.cycle_s != DEFAULT_CYCLE_S,
        "decode_duty": args.decode_duty != DEFAULT_DECODE_DUTY,
        "history_tokens": args.history_tokens > 0,
    }
    unsupported = [knob for knob in OPTIONAL_KNOBS if given[knob] and knob not in knobs]
    if unsupported:
        accepted = ", ".join(sorted(knobs)) or "<none>"
        raise ShapeError(
            f"scenario {shape.NAME!r} does not consume "
            f"{', '.join('--' + knob.replace('_', '-') for knob in unsupported)} "
            f"(knobs it accepts: {accepted})"
        )


def _validate(shape: ModuleType, plan: Any, args: ShapeArgs) -> None:
    if not isinstance(plan, ShapePlan):
        raise ShapeError(
            f"scenario {shape.NAME!r}: plan() must return a ShapePlan, "
            f"got {type(plan).__name__}"
        )
    ids = [model_id for model_id, _ in plan.models]
    if len(set(ids)) != len(ids):
        raise ShapeError(f"scenario {shape.NAME!r}: duplicate model ids {ids}")
    if len(plan.session_models) != args.sessions:
        raise ShapeError(
            f"scenario {shape.NAME!r}: session_models has "
            f"{len(plan.session_models)} entries for {args.sessions} session(s)"
        )
    undeclared = sorted(set(plan.session_models) - set(ids))
    if undeclared:
        raise ShapeError(
            f"scenario {shape.NAME!r}: session models {undeclared} are not declared "
            f"in models ({ids})"
        )
    if not plan.tools:
        raise ShapeError(f"scenario {shape.NAME!r}: the plan declares no tools")
    if plan.window_mode not in WINDOW_MODES:
        raise ShapeError(
            f"scenario {shape.NAME!r}: window_mode {plan.window_mode!r} is not one of "
            f"{WINDOW_MODES}"
        )
    if plan.window_mode == "hold" and plan.subscribe:
        raise ShapeError(
            f"scenario {shape.NAME!r}: window_mode 'hold' must not subscribe "
            "(a subscription pins the session and blocks eviction)"
        )
    if plan.window_mode == "hold" and plan.drives_upstream:
        raise ShapeError(
            f"scenario {shape.NAME!r}: window_mode 'hold' drives nothing, so it "
            "cannot declare drives_upstream"
        )
    if plan.window_mode == "hydrate" and not plan.subscribe:
        raise ShapeError(
            f"scenario {shape.NAME!r}: window_mode 'hydrate' needs subscriptions "
            "(hydration is measured by the sync_session a subscription produces)"
        )
    if plan.sample_interval_s <= 0:
        raise ShapeError(f"scenario {shape.NAME!r}: sample_interval_s must be > 0")


def _with_seed(plan: ShapePlan, args: ShapeArgs) -> ShapePlan:
    """挂上「每会话一轮大输出」的历史预置（沿用形态已有的 model 声明）。"""
    chars = max(args.chunk, math.ceil(args.history_tokens * HISTORY_CHARS_PER_TOKEN))
    turn = Turn.of(
        text="y" * chars,
        chunk=min(chars, SEED_FRAME_CHARS),
        delay=0.0,
    )
    seed = SeedSpec(
        model=SEED_MODEL,
        # 循环剧本：整档每会话一轮，轮数由会话数（而不是剧本长度）决定。
        script=CyclicScript(turn),
        tokens_per_session=args.history_tokens,
        chars_per_session=chars,
    )
    models: list[tuple[str, Script]] = list(plan.models)
    if all(model_id != SEED_MODEL for model_id, _ in models):
        models.append((SEED_MODEL, seed.script))
    # 用 replace 而不是逐字段重建：ShapePlan 加字段时这里不会静默丢掉它。
    return replace(plan, models=tuple(models), seed=seed)


def model_ids(plan: ShapePlan) -> list[str]:
    """给 ``ProbeEnv(models=…)`` 的声明列表（顺序稳定，便于比对产物）。"""
    return [model_id for model_id, _ in plan.models]


def register(env: Any, plan: ShapePlan) -> None:
    """把剧本登记到假 Provider（``ProbeEnv.register`` 的批量化）。"""
    for model_id, script in plan.models:
        env.register(model_id, script)


__all__ = [
    "CyclicScript",
    "DEFAULT_CHUNK",
    "DEFAULT_CYCLE_S",
    "DEFAULT_DECODE_DUTY",
    "DEFAULT_TOOL",
    "HISTORY_CHARS_PER_TOKEN",
    "OPTIONAL_KNOBS",
    "SEED_FRAME_CHARS",
    "SEED_MODEL",
    "SHAPES",
    "SeedSpec",
    "ShapeArgs",
    "ShapeError",
    "ShapePlan",
    "WINDOW_MODES",
    "build_plan",
    "covering_turn",
    "model_ids",
    "names",
    "prompt_key",
    "prompt_text_of",
    "register",
    "resolve",
    "seed_key",
]
