#!/usr/bin/env python3
"""负载形态：工具循环（``tool_cycle``）——测**每轮固定成本**。

每会话循环一条「一轮」：客户端发 prompt → 模型吐一个工具调用 → 网关执行工具 →
第二次模型调用吐一行正文 → ``turn_result``。于是采样窗口里每一轮都要走一遍
「提交 + 落盘 + 下一次请求组装 + 审计 INSERT」，``cpu_cores / turns_per_second``
就是每轮固定成本这个常数（与 ``pure_stream`` 的每事件成本相差两个数量级）。

工具默认 ``Read``（读 workspace 里的小文件，无子进程）；``--tool bash`` 换成
``Bash echo``（带子进程 + 进程组收尾）。``--cycle-s`` 通过「调用轮首个内容帧之前
的 ``first_delay``」把周期撑到**不短于**它：周期 = ``cycle_s`` + 该轮的真实成本，
所以 pacer 不污染每轮成本的读数；``--cycle-s 0`` = 不 pacing，压到机器能吃下的
最大轮次频率。

**每会话一个 model id**：假 Provider 的剧本游标按 model 走，而「调用轮 → 文本轮」
两段的类型不同——共享游标时会话之间一旦漂移一个请求，某个会话就会把文本轮当成
调用轮，工具不再执行、轮长变成两种的混合。见 ``shapes.ShapePlan``。
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from shapes import CyclicScript, ShapeArgs, ShapePlan
from wing_probe.provider.script import ToolCall, Turn

NAME = "tool_cycle"
MODEL = f"capacity/{NAME}"
KNOBS: frozenset[str] = frozenset({"tool", "cycle_s"})

#: ``--tool`` 的两个变体：工具名 / 参数 / 需要预置的 workspace 文件 / create 期覆盖。
TOOLS: Mapping[str, Mapping[str, Any]] = {
    "read": {
        "tool": "Read",
        "arguments": {"path": "note.txt"},
        "workspace_files": {"note.txt": "wing capacity probe\none short line\n"},
        "agent": {},
    },
    "bash": {
        "tool": "Bash",
        "arguments": {"command": "echo wing"},
        "workspace_files": {},
        "agent": {"yolo": True},
        # ``safe_command_patterns`` 为空时任何 Bash 命令都要人确认，而 headless
        # 测量里没有人回答（feedback 等待 6000 s）。create 期 yolo 是公开路径。
    },
}

#: 每轮结束时的正文（一个事件 = ``--chunk`` 个字符）。
REPLY_CHARS = 1


def cycle_turns(args: ShapeArgs, tool: str) -> tuple[Turn, Turn]:
    """一轮的两段：调用轮（首帧前按 ``--cycle-s`` pacing）+ 一行正文。

    混合形态复用同一份装配，两个形态的轮次语义因此不会分叉。
    """
    if tool not in TOOLS:
        raise ValueError(
            f"unknown --tool {tool!r}; available: {', '.join(sorted(TOOLS))}"
        )
    variant = TOOLS[tool]
    call_turn = Turn.of(
        tool_calls=[ToolCall(variant["tool"], dict(variant["arguments"]))],
        first_delay=args.cycle_s if args.cycle_s > 0 else None,
    )
    reply_turn = Turn.of(text="." * REPLY_CHARS * args.chunk, chunk=args.chunk)
    return call_turn, reply_turn


def plan(args: ShapeArgs) -> ShapePlan:
    call_turn, reply_turn = cycle_turns(args, args.tool)
    variant = TOOLS[args.tool]
    models: list[tuple[str, Any]] = []
    session_models: list[str] = []
    for index in range(args.sessions):
        model_id = f"{MODEL}/s{index}"
        models.append((model_id, CyclicScript(call_turn, reply_turn)))
        session_models.append(model_id)
    return ShapePlan(
        drives_upstream=True,
        turn_boundaries=True,
        models=tuple(models),
        session_models=tuple(session_models),
        tools=(str(variant["tool"]),),
        agent=dict(variant["agent"]),
        workspace_files=dict(variant["workspace_files"]),
    )
