"""wing-orch CLI — Goal 编排命令行入口。"""

from __future__ import annotations

import argparse
import asyncio
import logging
import sys
import uuid

from wing_orch.runner import GoalRunner


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="wing-orch",
        description="Wing orchestration — background Goal loop (executor/checker)",
    )
    sub = parser.add_subparsers(dest="command")

    goal_parser = sub.add_parser("goal", help="Run a Goal orchestration loop")

    # 目标输入
    goal_parser.add_argument(
        "prompt",
        nargs="?",
        default=None,
        help="Goal prompt text (or use --task-file)",
    )
    goal_parser.add_argument(
        "--task-file",
        metavar="PATH",
        help="Read goal prompt from file (for long goals)",
    )

    # 连接
    goal_parser.add_argument(
        "--gateway",
        default="http://127.0.0.1:32523",
        help="Gateway URL (default: http://127.0.0.1:32523)",
    )
    goal_parser.add_argument(
        "--api-key", default=None, help="API key for authentication"
    )
    goal_parser.add_argument(
        "--client-id",
        default=None,
        help="Tool host client ID (default: auto-generated)",
    )

    # 工作目录
    goal_parser.add_argument(
        "--workspace", default=".", help="Working directory for tools"
    )

    # 轮次
    goal_parser.add_argument(
        "--max-rounds",
        type=int,
        default=0,
        help="Max executor/checker rounds (0 = infinite, default)",
    )

    # 模型
    goal_parser.add_argument(
        "--executor-model", default=None, help="Executor model name"
    )
    goal_parser.add_argument("--checker-model", default=None, help="Checker model name")

    # 工具
    goal_parser.add_argument(
        "--executor-tools",
        default=None,
        help="Executor tools (comma-separated, default: all standard)",
    )
    goal_parser.add_argument(
        "--checker-tools",
        default=None,
        help="Checker tools (comma-separated, default: Bash,Read,Glob,Grep)",
    )

    # 系统提示词
    goal_parser.add_argument(
        "--executor-append-system-prompt",
        default=None,
        help="Append to executor system prompt",
    )
    goal_parser.add_argument(
        "--checker-append-system-prompt",
        default=None,
        help="Append to checker system prompt",
    )
    goal_parser.add_argument(
        "--checker-system-prompt",
        default=None,
        help="Override checker system prompt entirely",
    )

    # 状态
    goal_parser.add_argument(
        "--state-file",
        default=".wing-orch.json",
        help="Goal state persistence file (default: .wing-orch.json)",
    )
    goal_parser.add_argument(
        "--resume",
        action="store_true",
        help="Resume from state file",
    )

    # 日志
    goal_parser.add_argument("--log-file", default=None, help="Log output file")

    args = parser.parse_args()

    if args.command != "goal":
        parser.print_help()
        sys.exit(1)

    # 解析 goal prompt
    prompt = args.prompt
    if args.task_file:
        with open(args.task_file) as f:
            prompt = f.read().strip()
    if not prompt:
        goal_parser.error("provide a goal prompt or --task-file")

    # 解析工具列表
    executor_tools = (
        [t.strip() for t in args.executor_tools.split(",")]
        if args.executor_tools
        else None
    )
    checker_tools = (
        [t.strip() for t in args.checker_tools.split(",")]
        if args.checker_tools
        else None
    )

    # 配置日志
    _setup_logging(args.log_file)

    client_id = args.client_id or f"wing-orch-{uuid.uuid4().hex[:8]}"

    runner = GoalRunner(
        goal_prompt=prompt,
        gateway_url=args.gateway,
        api_key=args.api_key,
        client_id=client_id,
        workspace=args.workspace,
        max_rounds=args.max_rounds,
        executor_model=args.executor_model,
        checker_model=args.checker_model,
        executor_tools=executor_tools,
        checker_tools=checker_tools,
        executor_append_system_prompt=args.executor_append_system_prompt,
        checker_append_system_prompt=args.checker_append_system_prompt,
        checker_system_prompt=args.checker_system_prompt,
        state_file=args.state_file,
        resume=args.resume,
    )

    try:
        asyncio.run(runner.run())
    except KeyboardInterrupt:
        sys.exit(130)
    except RuntimeError as e:
        logging.getLogger("wing-orch").error(str(e))
        sys.exit(1)

    if runner.interrupted:
        sys.exit(130)


def _setup_logging(log_file: str | None) -> None:
    handlers: list[logging.Handler] = [logging.StreamHandler(sys.stderr)]
    if log_file:
        handlers.append(logging.FileHandler(log_file, encoding="utf-8"))

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
        datefmt="%H:%M:%S",
        handlers=handlers,
    )


if __name__ == "__main__":
    main()
