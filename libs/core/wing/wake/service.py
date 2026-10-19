"""wing/wake/service.py — 唤醒服务：校验、生命周期、事件分发、投递、重启恢复。

三块职责的协调者（存储 / 调度 / 信封在各自模块里，这里只编排）：

  - **创建**：校验（可解析的 source / target、非自指、spec 与 times 的合法组合）→ 落库 → 叫醒调度器；
  - **触发**：事件驱动（``TurnResultEvent`` / ``AskEvent``，**同步 handler 只投递 task**）与时间驱动
    （调度器到点调 :meth:`fire_timer`）；
  - **投递**：``ports.deliver``（= ``runtime.post``，带新 ``request_id``）；失败一律 ``state=failed``
    + 一行 WARN，**异常绝不外溢**——唤醒失败永不影响 source 会话。

不变量：

  - done 信封送达后 done 配额 −1，归零即 ``state=done``（整条终结，不再留 ask 空转）；
  - ask 事件不消耗配额；
  - 三态终局（done / canceled / failed）不再武装、不再重试；
  - 错过的定时触发只补偿一次（一次性补发、周期型重新对齐），绝不追债式连发。
"""

from __future__ import annotations

import asyncio
import time
import uuid
from typing import Callable

from wing.common.logger import log
from wing.db import Database
from wing.event import AskEvent, TurnResultEvent, WingEvent
from wing.event_bus import event_bus
from wing.wake import envelope
from wing.wake.model import (
    DEFAULT_ON_EVENTS,
    EVENT_ASK,
    EVENT_DONE,
    KIND_SESSION,
    KINDS,
    STATE_FAILED,
    WakeTrigger,
    parse_on_events,
)
from wing.wake.ports import WakePorts
from wing.wake.scheduler import WakeScheduler
from wing.wake.spec import (
    SpecError,
    WakeSpec,
    advance_fire_at_ms,
    first_fire_at_ms,
    parse_spec,
)
from wing.wake.store import WakeStore, WakeWriteLost

#: 失败文案的落库上限（``last_error``）：异常可能带很长的栈描述，库列只留结论。
ERROR_LIMIT = 500


class WakeUnavailableError(RuntimeError):
    """``wing.db`` 不可用——唤醒触发器无法持久化（路由回 503）。

    与「不武装」配套：DB 打不开时 ``install()`` 什么都不做（订阅都不装），创建因此必须
    如实失败，而不是返回一条谁也读不到的触发器。
    """


def _default_clock_ms() -> int:
    return int(time.time() * 1000)


class WakeService:
    """唤醒触发器服务（进程单例，由 ``WingRuntime`` 持有）。"""

    def __init__(
        self,
        ports: WakePorts,
        *,
        database: Database | None = None,
        clock: Callable[[], int] = _default_clock_ms,
    ) -> None:
        self._ports = ports
        self._clock = clock
        self._store = WakeStore(database)
        self._scheduler = WakeScheduler(self, clock=clock)
        self._installed = False
        self._stalled: set[int] = set()
        """本进程**暂不重试**的触发器 id（记账写失败，见 :meth:`_stall`）。"""

    # ── 生命周期 ─────────────────────────────────

    def install(self) -> None:
        """订阅 EventBus（幂等）。DB 不可用 → 一行 ERROR + **整体不武装**。

        同步、无循环依赖：由 ``WingRuntime.__init__`` 调用（那里没有事件循环）。
        """
        if self._installed:
            return
        database = self._store.database()
        if not database.ready:
            log.error(f"wake disabled: wing.db unavailable at {database.path}")
            return
        event_bus.subscribe(self._on_event)
        self._installed = True
        log.info("wake service installed")

    def start(self) -> None:
        """启动调度循环（幂等；需在运行中的事件循环内调用）。

        与 ``BackgroundScheduler.start`` 同层生命周期：由 gateway lifespan 调用。第一棒是
        重启恢复（见 :meth:`recover_missed`）——它要 await 投递，只能在循环里跑。
        """
        if not self._installed:
            return
        self._scheduler.start()

    async def stop(self) -> None:
        """停止调度循环（幂等；lifespan shutdown 调用）。"""
        await self._scheduler.stop()

    # ── 创建 / 取消 / 查询 ───────────────────────

    def create(
        self,
        *,
        kind: str,
        target_session_id: str,
        source_session_id: str | None = None,
        on_events: str | None = None,
        spec: str | None = None,
        prompt: str | None = None,
        times: int | None = None,
        created_by: str | None = None,
    ) -> WakeTrigger:
        """校验 + 落库 + 叫醒调度器。

        Raises:
            ValueError: kind / on_events / spec / times 非法或组合非法（含 target == source）
            LookupError: source 或 target 不可解析（不是会话 / 已归档 / id 不合规）
            WakeUnavailableError: ``wing.db`` 不可用
        """
        database = self._store.database()
        if not database.ready:
            raise WakeUnavailableError(
                f"wake unavailable: wing.db not usable at {database.path}"
            )

        kind = (kind or "").strip()
        if kind not in KINDS:
            raise ValueError(f"unknown wake kind {kind!r} (expected session|timer)")

        target = (target_session_id or "").strip()
        self._require_session(target, field="target_session_id")

        events = parse_on_events(DEFAULT_ON_EVENTS if on_events is None else on_events)

        if kind == KIND_SESSION:
            source = (source_session_id or "").strip()
            if not source:
                raise ValueError("session wake requires source_session_id")
            if spec:
                raise ValueError("spec only applies to timer wakes")
            if source == target:
                raise ValueError(
                    "source_session_id and target_session_id must differ"
                    "（唤醒自己会立刻再触发一轮，不做防环）"
                )
            self._require_session(source, field="source_session_id")
            budget = self._budget(times, default=1)
            next_fire_at_ms = None
            spec_text = None
        else:
            if source_session_id:
                raise ValueError("timer wake does not take source_session_id")
            parsed = self._parse_spec(spec)
            if not (prompt or "").strip():
                raise ValueError("timer wake requires a prompt")
            if times is not None and parsed.one_shot:
                raise ValueError(
                    f"times only applies to recurring specs (every/daily);"
                    f" {parsed.kind} fires once"
                )
            budget = self._budget(times, default=1 if parsed.one_shot else None)
            next_fire_at_ms = first_fire_at_ms(parsed, self._clock())
            spec_text = parsed.text

        trigger = self._store.insert(
            kind=kind,
            target_session_id=target,
            source_session_id=(source if kind == KIND_SESSION else None),
            on_events=events,
            spec=spec_text,
            next_fire_at_ms=next_fire_at_ms,
            remaining_done=budget,
            prompt=(prompt if prompt and prompt.strip() else None),
            created_at_ms=self._clock(),
            created_by=created_by,
        )
        if trigger is None:
            raise WakeUnavailableError("wake unavailable: failed to persist trigger")
        log.info(
            f"wake: created #{trigger.id} kind={kind} target={target} "
            f"source={trigger.source_session_id or '-'} spec={spec_text or '-'}"
        )
        self._scheduler.notify()
        return trigger

    def cancel(self, trigger_id: int) -> bool:
        """取消一条触发器。

        Returns:
            ``True`` = 本次取消生效；``False`` = 已终结（幂等 no-op）。

        Raises:
            LookupError: 该 id 不存在
            WakeUnavailableError: 状态写没落库（"取消没生效"不是"它早完了"）
        """
        try:
            canceled = self._store.cancel(trigger_id)
        except WakeWriteLost as exc:
            raise WakeUnavailableError(f"wake unavailable: {exc}") from exc
        if canceled is None:
            raise LookupError(f"wake trigger not found: {trigger_id}")
        if canceled:
            log.info(f"wake: canceled #{trigger_id}")
            self._scheduler.notify()
        return canceled

    def list_triggers(self, *, include_finished: bool = False) -> list[WakeTrigger]:
        """列出触发器（``include_finished`` 为假只看 ``active``）。"""
        return self._store.list_triggers(include_finished=include_finished)

    def cancel_for_session(self, session_id: str) -> int:
        """归档联动：source 或 target 命中该会话的 ``active`` 触发器全部取消。"""
        canceled = self._store.cancel_for_session(session_id)
        if canceled:
            log.info(f"wake: canceled {canceled} trigger(s) for session {session_id}")
            self._scheduler.notify()
        return canceled

    # ── 调度器接口 ───────────────────────────────

    def due_timers(self, now_ms: int) -> list[WakeTrigger]:
        return self._store.due_timers(now_ms, exclude=frozenset(self._stalled))

    def next_due_ms(self) -> int | None:
        return self._store.next_due_ms(exclude=frozenset(self._stalled))

    async def fire_timer(self, trigger: WakeTrigger, now_ms: int) -> None:
        """到点触发一条 timer：投递 → 记账（配额 / 下次排期 / 终态）。"""
        content = envelope.timer_envelope(
            spec=trigger.spec or "",
            ordinal=trigger.fire_count + 1,
            prompt=trigger.prompt or "",
        )
        if not await self._deliver(trigger, content):
            return
        try:
            parsed = parse_spec(trigger.spec or "")
        except SpecError as exc:  # 库里的 spec 坏了：一次性的事故，别让它每 tick 重试
            # `exc` 在 except 块结束时被删除（Python 语义）：先取出文案再闭包。
            error = str(exc)
            self._write(
                trigger.id,
                "spec failed",
                lambda: self._store.set_state(trigger.id, STATE_FAILED, error=error),
            )
            return
        # 一次性 spec 的预算是 1（创建期落库），所以"预算归零即终结"对它与对
        # `--times N` 的周期型是同一条规则——记账在一条 SQL 里原子完成（见 store）。
        self._write(
            trigger.id,
            "record fire",
            lambda: self._store.record_fire(
                trigger.id,
                fired_at_ms=now_ms,
                decrement=1,
                next_fire_at_ms=advance_fire_at_ms(parsed, now_ms),
            ),
        )
        self._scheduler.notify()

    async def recover_missed(self) -> int:
        """重启恢复：处理启动时已经错过的 timer（详见 ``docs/dev/wake.md``）。

        - 一次性（``in`` / ``at``）：立刻补发一次，然后 ``done``；
        - 周期型（``every`` / ``daily``）：**不补发**，从 now 重新对齐下一次。

        多条 missed 不会连发：一次性只有一次投递，周期型一次都不投。

        Returns:
            本次处理的（曾错过的）触发器条数。
        """
        now = self._clock()
        missed = [
            trigger
            for trigger in self._store.due_timers(now)
            if trigger.id not in self._stalled
        ]
        for trigger in missed:
            try:
                parsed = parse_spec(trigger.spec or "")
            except SpecError as exc:
                error = str(exc)
                self._write(
                    trigger.id,
                    "spec failed",
                    lambda: self._store.set_state(
                        trigger.id, STATE_FAILED, error=error
                    ),
                )
                continue
            if parsed.one_shot:
                log.info(f"wake: compensating missed one-shot #{trigger.id}")
                await self.fire_timer(trigger, now)
            else:
                aligned = advance_fire_at_ms(parsed, now)
                self._write(
                    trigger.id,
                    "realign",
                    lambda: self._store.reschedule(trigger.id, aligned),
                )
                log.info(
                    f"wake: realigned recurring #{trigger.id} "
                    f"(missed, no catch-up) next={aligned}"
                )
        return len(missed)

    # ── 事件驱动 ─────────────────────────────────

    def _on_event(self, event: WingEvent) -> None:
        """EventBus 订阅者（**同步**）：只判类型 + 投递 task，绝不在 emit 栈里投递。

        在 emit 调用栈里同步投递等于让 source 的收口写入和 target 的一整轮 LLM 抢同一次
        调用栈——收口记录必须先落地（``create_task`` 把它推到下个 tick）。
        """
        if not self._installed:
            return
        if isinstance(event, TurnResultEvent):
            kind = EVENT_DONE
        elif isinstance(event, AskEvent):
            kind = EVENT_ASK
        else:
            return
        session_id = event.session_id
        if not session_id:
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            log.warning(
                f"wake: no running event loop; dropped {kind} wake for {session_id}"
            )
            return
        loop.create_task(self._dispatch(kind, session_id, event))

    async def _dispatch(self, kind: str, session_id: str, event: WingEvent) -> None:
        """把一条事件投给所有订阅它的 ``active`` 触发器（单条失败不影响其它）。"""
        try:
            triggers = [
                trigger
                for trigger in self._store.active_for_source(session_id)
                if kind in trigger.events and trigger.id not in self._stalled
            ]
        except Exception:
            log.exception(f"wake: failed to load triggers for {session_id}")
            return
        for trigger in triggers:
            try:
                await self._dispatch_one(trigger, kind, event)
            except Exception:
                log.exception(f"wake: dispatch for #{trigger.id} failed")

    async def _dispatch_one(
        self, trigger: WakeTrigger, kind: str, event: WingEvent
    ) -> None:
        if not trigger.active:
            return
        if kind == EVENT_DONE:
            assert isinstance(event, TurnResultEvent)
            envelope_text = envelope.done_envelope(
                session_id=trigger.source_session_id or "",
                subtype=event.subtype,
                num_turns=event.num_turns,
                duration_ms=event.duration_ms,
                result=event.result,
            )
        else:
            assert isinstance(event, AskEvent)
            envelope_text = envelope.ask_envelope(
                session_id=trigger.source_session_id or "",
                tool_call_id=event.tool_call_id,
                questions=event.questions,
                question=event.question,
                choices=event.choices,
            )
        content = envelope.session_payload(trigger, envelope_text)
        if not await self._deliver(trigger, content):
            return

        # done 事件扣一次配额（归零即终结）；ask 事件不扣（decrement=0）。
        self._write(
            trigger.id,
            "record fire",
            lambda: self._store.record_fire(
                trigger.id,
                fired_at_ms=self._clock(),
                decrement=1 if kind == EVENT_DONE else 0,
                next_fire_at_ms=None,
            ),
        )

    # ── 投递 ─────────────────────────────────────

    async def _deliver(self, trigger: WakeTrigger, content: str) -> bool:
        """投递一条消息；任何异常都折成 ``state=failed`` + 一行 WARN（绝不外溢）。"""
        try:
            await self._ports.deliver(
                session_id=trigger.target_session_id,
                content=content,
                request_id=uuid.uuid4().hex,
            )
        except Exception as exc:
            reason = self._describe_failure(trigger.target_session_id, exc)
            log.warning(
                f"wake: deliver #{trigger.id} → {trigger.target_session_id} failed: {reason}"
            )
            self._write(
                trigger.id,
                "record failure",
                lambda: self._store.set_state(
                    trigger.id, STATE_FAILED, error=reason[:ERROR_LIMIT]
                ),
            )
            return False
        return True

    def _write(self, trigger_id: int, what: str, write: Callable[[], bool]) -> None:
        """执行一次记账写入；**写不进去就把这条触发器撤出重试**。

        ``wing.db`` 的契约是"写失败静默返回 False"（磁盘满 / 只读 / 长锁）。到点的
        timer 若记账没落库，状态上仍是 ``active AND next_fire_at_ms <= now``——
        调度器下一轮（``_MIN_TICK_SECONDS`` = 10ms）会把它再捞出来，于是同一条消息被
        反复 ``runtime.post`` 进目标会话（实测 ~83 次/秒）。投递成功但记不下来时，
        这一进程不再碰它：留一行 ERROR，本进程内永不重试（重启后按库里的旧排期重新
        判断，那时库可能已经好了）。
        """
        try:
            ok = write()
        except Exception:
            ok = False
        if ok:
            return
        self._stall(trigger_id, what)

    def _stall(self, trigger_id: int, what: str) -> None:
        if trigger_id in self._stalled:
            return
        self._stalled.add(trigger_id)
        log.error(
            f"wake: #{trigger_id} stalled ({what} failed: wing.db write lost); "
            f"it will not fire again in this process"
        )
        self._scheduler.notify()

    def _describe_failure(self, session_id: str, exc: BaseException) -> str:
        """ "已归档" 是可行动结论（提示恢复），其余异常如实带出类型与文案。"""
        if self._ports.is_archived(session_id):
            return f"target session {session_id} is archived (restore it to resume wake-ups)"
        return f"{type(exc).__name__}: {exc}"

    # ── 校验辅助 ─────────────────────────────────

    def _require_session(self, session_id: str, *, field: str) -> None:
        if not session_id:
            raise ValueError(f"{field} is required")
        if not self._ports.session_exists(session_id):
            if self._ports.is_archived(session_id):
                raise LookupError(f"{field} {session_id!r} is archived")
            raise LookupError(f"{field} {session_id!r} not found")

    def _parse_spec(self, spec: str | None) -> WakeSpec:
        if not (spec or "").strip():
            raise ValueError("timer wake requires a spec (in/at/every/daily)")
        return parse_spec(spec or "")

    @staticmethod
    def _budget(times: int | None, *, default: int | None) -> int | None:
        """``times`` → 剩余预算（``None`` = 无限，只有 timer 允许）。"""
        if times is None:
            return default
        if not isinstance(times, int):
            raise ValueError(f"times must be an integer, got {times!r}")
        if times <= 0:
            raise ValueError(f"times must be >= 1, got {times}")
        return times


__all__ = ["WakeService", "WakeUnavailableError"]
