"""wing-spawn CLI — hand a goal to a fresh disposable tool container.

Usage:
    wing-spawn "Implement X" --model gpt-4o                 # sync (await result)
    wing-spawn --task-file task.md --async --callback URL    # async (detach + notify)
    wing-spawn status <task_id>                              # observe progress
    wing-spawn list                                          # all tasks
    wing-spawn cancel <task_id>                              # cancel a running task
    wing-spawn cleanup                                       # remove leftover containers

The parent agent calls this to dispatch work to a fresh coding agent. Defaults
are read from the environment (WING_GATEWAY_URL, WING_ADMIN_KEY, WING_TOOL_KEY,
DEFAULT_MODEL, REGISTRY, TAG) so it works with zero config in the devbox.
"""

from __future__ import annotations

import argparse
import asyncio
import logging
import os
import sys

from wing_spawn.runner import SpawnRunner, DEFAULT_GATEWAY, DEFAULT_MODEL


def _add_run_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("prompt", nargs="?", default=None, help="Goal prompt text")
    parser.add_argument("--task-file", metavar="PATH", help="Read goal prompt from file")

    # Connection
    parser.add_argument(
        "--gateway",
        default=os.environ.get("WING_GATEWAY_URL", DEFAULT_GATEWAY),
        help=f"Gateway URL (default: {DEFAULT_GATEWAY})",
    )
    parser.add_argument(
        "--api-key",
        default=os.environ.get("WING_ADMIN_KEY"),
        help="Admin API key (env: WING_ADMIN_KEY)",
    )
    parser.add_argument(
        "--tool-key",
        default=os.environ.get("WING_TOOL_KEY"),
        help="Tool_runtime API key the container uses (env: WING_TOOL_KEY)",
    )

    # Container
    parser.add_argument(
        "--client-id",
        default=None,
        help="Tool container namespace (default: auto-generated task-<hex>)",
    )
    parser.add_argument(
        "--image",
        default=None,
        help="Tool container image (default: $REGISTRY/wing-devbox:$TAG)",
    )
    parser.add_argument(
        "--workspace",
        default=os.environ.get("WING_WORKSPACE"),
        help="Workspace path (default: $WING_WORKSPACE)",
    )
    parser.add_argument(
        "--network",
        default=None,
        help="Docker network (default: share the parent's network stack)",
    )

    # Agent
    parser.add_argument(
        "--model",
        default=os.environ.get("DEFAULT_MODEL", DEFAULT_MODEL),
        help=f"Executor model (default: {DEFAULT_MODEL})",
    )
    parser.add_argument(
        "--tools",
        default=None,
        help="Comma-separated tool refs (default: <client_id>.<standard tools>)",
    )
    parser.add_argument(
        "--append-system-prompt",
        default=None,
        help="Extra text to append to the executor system prompt",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=1800.0,
        help="Seconds to wait for a result (default: 1800)",
    )
    parser.add_argument(
        "--keep",
        action="store_true",
        help="Keep the tool container after the run (default: remove it)",
    )

    # Async / observability
    parser.add_argument(
        "--async",
        dest="async_mode",
        action="store_true",
        help="Submit and return immediately (detach a background watcher)",
    )
    parser.add_argument(
        "--callback",
        default=None,
        help="URL to POST the task outcome to when it finishes (async mode)",
    )
    parser.add_argument("--log-level", default="INFO", help="Log level")


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="wing-spawn",
        description="Spawn a disposable tool container and run a goal against it.",
    )
    sub = parser.add_subparsers(dest="command")

    # ── run (default) ─────────────────────────────────────────
    run_parser = sub.add_parser("run", help="Run a goal against a fresh container (default)")
    _add_run_args(run_parser)

    # ── watch (internal, background watcher) ──────────────────
    watch_parser = sub.add_parser(
        "watch", help="Internal: wait for a submitted task's result (background)"
    )
    watch_parser.add_argument("task_id", help="Task id to watch")
    watch_parser.add_argument("--api-key", default=os.environ.get("WING_ADMIN_KEY"))
    watch_parser.add_argument("--timeout", type=float, default=1800.0)
    watch_parser.add_argument("--keep", action="store_true")
    watch_parser.add_argument("--log-level", default="INFO")

    # ── status / list / cancel ────────────────────────────────
    sub.add_parser("list", help="List all submitted tasks")
    status_parser = sub.add_parser("status", help="Show a task's status")
    status_parser.add_argument("task_id", help="Task id")
    cancel_parser = sub.add_parser("cancel", help="Cancel a running task")
    cancel_parser.add_argument("task_id", help="Task id")

    # ── cleanup ───────────────────────────────────────────────
    sub.add_parser("cleanup", help="Remove all leftover wing-spawn containers")

    # Backwards-compatible: bare `wing-spawn "prompt"` / `wing-spawn --flag …`
    # without a subcommand defaults to `run`.
    argv = sys.argv[1:]
    if argv and argv[0] not in ("run", "watch", "list", "status", "cancel", "cleanup"):
        argv = ["run", *argv]

    args = parser.parse_args(argv)

    if args.command == "cleanup":
        _cleanup()
        return
    if args.command == "list":
        _list()
        return
    if args.command == "status":
        _status(args.task_id)
        return
    if args.command == "cancel":
        _cancel(args.task_id)
        return
    if args.command == "watch":
        _watch(args)
        return
    if args.command == "run":
        _run(parser, args)
        return

    parser.print_help()


def _cleanup() -> None:
    from wing_spawn.containers import cleanup_all

    n = cleanup_all()
    print(f"Removed {n} leftover wing-spawn container(s).")


def _list() -> None:
    from wing_spawn.state import list_tasks

    tasks = list_tasks()
    if not tasks:
        print("(no tasks)")
        return
    print(f"{'TASK ID':<22} {'STATUS':<10} {'MODEL':<28} CREATED")
    for t in tasks:
        created = __import__("datetime").datetime.fromtimestamp(t.created_at).strftime(
            "%H:%M:%S"
        )
        print(f"{t.task_id:<22} {t.status:<10} {t.model:<28} {created}")


def _status(task_id: str) -> None:
    from wing_spawn.state import load

    t = load(task_id)
    if t is None:
        print(f"no such task: {task_id}")
        sys.exit(1)
    print(f"task_id:    {t.task_id}")
    print(f"client_id:  {t.client_id}")
    print(f"status:     {t.status}")
    print(f"model:      {t.model}")
    print(f"session:    {t.session_id}")
    print(f"created:    {__import__('datetime').datetime.fromtimestamp(t.created_at)}")
    print(f"updated:    {__import__('datetime').datetime.fromtimestamp(t.updated_at)}")
    if t.error:
        print(f"error:      {t.error}")
    if t.result:
        print(f"---- result ----\n{t.result}")


def _cancel(task_id: str) -> None:
    from wing_spawn.containers import cleanup_container

    def _run() -> None:
        from wing_spawn.state import CANCELLED, load, save

        t = load(task_id)
        if t is None:
            print(f"no such task: {task_id}")
            return
        t.status = CANCELLED
        save(t)
        if t.client_id:
            cleanup_container(t.client_id)
        print(f"cancelled: {task_id}")

    asyncio.run(_run())


def _watch(args: argparse.Namespace) -> None:
    logging.basicConfig(
        level=getattr(logging, args.log_level.upper(), logging.INFO),
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
        datefmt="%H:%M:%S",
        stream=sys.stderr,
    )
    runner = SpawnRunner(
        "",  # goal not needed in watch mode
        api_key=args.api_key,
        timeout=args.timeout,
        keep=args.keep,
    )
    try:
        code = asyncio.run(runner.watch(args.task_id))
    except KeyboardInterrupt:
        code = 130
    sys.exit(code)


def _run(parser: argparse.ArgumentParser, args: argparse.Namespace) -> None:
    prompt = args.prompt
    if args.task_file:
        with open(args.task_file) as f:
            prompt = f.read().strip()
    if not prompt:
        parser.error("provide a goal prompt or --task-file")

    tools = ([t.strip() for t in args.tools.split(",")] if args.tools else None)

    logging.basicConfig(
        level=getattr(logging, args.log_level.upper(), logging.INFO),
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
        datefmt="%H:%M:%S",
        stream=sys.stderr,
    )

    runner = SpawnRunner(
        prompt,
        gateway_url=args.gateway,
        api_key=args.api_key,
        tool_key=args.tool_key,
        client_id=args.client_id,
        image=args.image,
        workspace=args.workspace,
        network=args.network,
        model=args.model,
        tools=tools,
        append_system_prompt=args.append_system_prompt,
        timeout=args.timeout,
        keep=args.keep,
        async_mode=args.async_mode,
        callback_url=args.callback,
    )

    try:
        code = asyncio.run(runner.run())
    except KeyboardInterrupt:
        code = 130
    sys.exit(code)


if __name__ == "__main__":
    main()