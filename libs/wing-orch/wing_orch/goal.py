"""Goal 编排状态机——port from crates/wing/src/app/goal.rs。

纯逻辑，无 I/O。所有方法返回 list[GoalAction]，由 runner 翻译为副作用。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any


class GoalRole(Enum):
    EXECUTOR = "executor"
    CHECKER = "checker"

    @property
    def label(self) -> str:
        return "Executor" if self == GoalRole.EXECUTOR else "Checker"


class GoalPhase(Enum):
    CREATING_CHECKER = "creating_checker"
    EXECUTOR_WORKING = "executor_working"
    CHECKER_WORKING = "checker_working"
    INTERRUPTED = "interrupted"  # 附带 role
    COMPLETED = "completed"


@dataclass
class GoalAction:
    """状态机产出的动作。"""

    kind: str  # send_executor | send_checker | create_checker | goal_complete | exit
    content: str = ""
    reason: str = ""
    system_prompt: str = ""


# ── 常量 ──────────────────────────────────────────────────────

MAX_FORMAT_RETRIES = 2

DEFAULT_CHECKER_SYSTEM_PROMPT = """You are a Goal Checker. Your role is to VERIFY whether a task has been completed correctly. You are NOT the executor — do NOT attempt to do the task yourself.

You will receive:
1. A task goal (【任务目标】) describing what should be achieved
2. An execution result (【执行结果】) describing what the executor claims to have done

Your job:
- Use your tools (Bash, Read, Glob, Grep) to independently verify the execution result
- Check files exist, content is correct, commands work, etc.
- Do NOT modify any files or execute the task yourself

When you have finished checking, you MUST end your response with EXACTLY this format:

<goal_finish>true</goal_finish>
<reason>brief explanation of why the goal is met</reason>

OR if the goal is NOT met:

<goal_finish>false</goal_finish>
<reason>brief explanation of what is missing or wrong</reason>

The <goal_finish> tag is MANDATORY. Always include it in your final response."""

FORMAT_REMINDER = (
    "Your last response did not contain the required <goal_finish> tag. "
    "You MUST end your response with:\n\n"
    "<goal_finish>true</goal_finish>\n<reason>...</reason>\n\nOR\n\n"
    "<goal_finish>false</goal_finish>\n<reason>...</reason>\n\n"
    "Please re-state your conclusion with the correct format."
)


# ── 解析 ──────────────────────────────────────────────────────


def parse_goal_finish(text: str) -> bool | None:
    """解析 <goal_finish>...</goal_finish>（取最后出现）。

    Returns: True/False if tag found, None if not found.
    """
    idx = text.rfind("<goal_finish>")
    if idx == -1:
        return None
    after = text[idx + len("<goal_finish>") :]
    end = after.find("</goal_finish>")
    if end == -1:
        return None
    value = after[:end].strip().lower()
    return value == "true"


def parse_reason(text: str) -> str | None:
    """解析 <reason>...</reason>（取最后出现）。"""
    idx = text.rfind("<reason>")
    if idx == -1:
        return None
    after = text[idx + len("<reason>") :]
    end = after.find("</reason>")
    if end == -1:
        return None
    return after[:end].strip()


# ── 状态机 ────────────────────────────────────────────────────


@dataclass
class GoalState:
    """Goal 编排状态机。"""

    executor_session_id: str
    goal_prompt: str
    checker_system_prompt: str = DEFAULT_CHECKER_SYSTEM_PROMPT

    phase: GoalPhase = GoalPhase.CREATING_CHECKER
    interrupted_role: GoalRole | None = None
    checker_session_id: str | None = None
    round: int = 1
    appends: list[str] = field(default_factory=list)
    format_retries: int = 0

    @classmethod
    def new(
        cls,
        executor_session_id: str,
        goal_prompt: str,
        checker_system_prompt: str = DEFAULT_CHECKER_SYSTEM_PROMPT,
    ) -> tuple["GoalState", list[GoalAction]]:
        state = cls(
            executor_session_id=executor_session_id,
            goal_prompt=goal_prompt,
            checker_system_prompt=checker_system_prompt,
        )
        actions = [
            GoalAction(
                kind="create_checker",
                system_prompt=checker_system_prompt,
            ),
            GoalAction(
                kind="send_executor",
                content=state.build_prompt_user(),
            ),
        ]
        return state, actions

    def on_checker_created(self, session_id: str) -> list[GoalAction]:
        self.checker_session_id = session_id
        self.phase = GoalPhase.EXECUTOR_WORKING
        return []

    def on_checker_create_failed(self) -> list[GoalAction]:
        self.phase = GoalPhase.COMPLETED
        return [GoalAction(kind="exit")]

    def on_turn_result(self, role: GoalRole, result: str | None) -> list[GoalAction]:
        expected = (
            GoalPhase.EXECUTOR_WORKING
            if role == GoalRole.EXECUTOR
            else GoalPhase.CHECKER_WORKING
        )
        if self.phase != expected:
            return []

        if role == GoalRole.EXECUTOR:
            return self._on_executor_done(result or "")
        return self._on_checker_done(result or "")

    def on_interrupted(self, role: GoalRole) -> list[GoalAction]:
        dominated = (
            self.phase == GoalPhase.EXECUTOR_WORKING and role == GoalRole.EXECUTOR
        ) or (self.phase == GoalPhase.CHECKER_WORKING and role == GoalRole.CHECKER)
        if dominated:
            self.phase = GoalPhase.INTERRUPTED
            self.interrupted_role = role
        return []

    @property
    def is_working(self) -> bool:
        return self.phase in (GoalPhase.EXECUTOR_WORKING, GoalPhase.CHECKER_WORKING)

    # ── 消息构建 ──────────────────────────────────────────────

    def build_prompt_user(self) -> str:
        text = f"【任务目标】\n{self.goal_prompt}"
        if self.appends:
            text += "\n【追加信息】\n"
            for i, append in enumerate(self.appends, 1):
                text += f"{i}. {append}"
                if i < len(self.appends):
                    text += "\n"
        return text

    def build_checker_message(self, exec_result: str) -> str:
        return f"{self.build_prompt_user()}\n\n【执行结果】\n{exec_result}"

    def build_executor_message(self, checker_feedback: str) -> str:
        return f"{self.build_prompt_user()}\n\n【检查者反馈】\n{checker_feedback}"

    # ── 序列化（state-file 持久化）────────────────────────────

    def to_dict(self) -> dict[str, Any]:
        return {
            "executor_session_id": self.executor_session_id,
            "checker_session_id": self.checker_session_id,
            "goal_prompt": self.goal_prompt,
            "checker_system_prompt": self.checker_system_prompt,
            "phase": self.phase.value,
            "interrupted_role": self.interrupted_role.value
            if self.interrupted_role
            else None,
            "round": self.round,
            "appends": self.appends,
            "format_retries": self.format_retries,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "GoalState":
        state = cls(
            executor_session_id=d["executor_session_id"],
            goal_prompt=d["goal_prompt"],
            checker_system_prompt=d.get(
                "checker_system_prompt", DEFAULT_CHECKER_SYSTEM_PROMPT
            ),
        )
        state.checker_session_id = d.get("checker_session_id")
        state.phase = GoalPhase(d.get("phase", "executor_working"))
        ir = d.get("interrupted_role")
        state.interrupted_role = GoalRole(ir) if ir else None
        state.round = d.get("round", 1)
        state.appends = d.get("appends", [])
        state.format_retries = d.get("format_retries", 0)
        return state

    # ── 内部 ──────────────────────────────────────────────────

    def _on_executor_done(self, result: str) -> list[GoalAction]:
        checker_msg = self.build_checker_message(result)
        self.phase = GoalPhase.CHECKER_WORKING
        self.format_retries = 0
        return [
            GoalAction(kind="send_checker", content=checker_msg),
        ]

    def _on_checker_done(self, output: str) -> list[GoalAction]:
        finish = parse_goal_finish(output)

        if finish is True:
            reason = parse_reason(output) or ""
            self.phase = GoalPhase.COMPLETED
            return [
                GoalAction(kind="goal_complete", reason=reason),
                GoalAction(kind="exit"),
            ]

        if finish is False:
            executor_msg = self.build_executor_message(output)
            self.round += 1
            self.phase = GoalPhase.EXECUTOR_WORKING
            return [
                GoalAction(kind="send_executor", content=executor_msg),
            ]

        # 格式错误
        self.format_retries += 1
        if self.format_retries > MAX_FORMAT_RETRIES:
            self.phase = GoalPhase.INTERRUPTED
            self.interrupted_role = GoalRole.CHECKER
            return [
                GoalAction(kind="stall", reason="checker format error (max retries)")
            ]
        return [GoalAction(kind="send_checker", content=FORMAT_REMINDER)]
