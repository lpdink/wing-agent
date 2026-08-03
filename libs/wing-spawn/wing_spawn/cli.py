"""wing-spawn CLI — hand a goal to a fresh disposable tool container.

Usage:
    wing-spawn "Implement X" --model gpt-4o
    wing-spawn --task-file task.md --model deepseek-v4-flash-0731 --keep

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


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="wing-spawn",
        description="Spawn a disposable tool container and run a goal against it.",
    )
    sub = parser.add_subparsers(dest="command")

    # ── run (default) ─────────────────────────────────────────
    run_parser = sub.add_parser("run", help="Run a goal against a fresh container (default)")
    run_parser.add_argument("prompt", nargs="?", default=None, help="Goal prompt text")
    run_parser.add_argument("--task-file", metavar="PATH", help="Read goal prompt from file")

    # Connection
    run_parser.add_argument(
        "--gateway",
        default=os.environ.get("WING_GATEWAY_URL", DEFAULT_GATEWAY),
        help=f"Gateway URL (default: {DEFAULT_GATEWAY})",
    )
    run_parser.add_argument(
        "--api-key",
        default=os.environ.get("WING_ADMIN_KEY"),
        help="Admin API key (env: WING_ADMIN_KEY)",
    )
    run_parser.add_argument(
        "--tool-key",
        default=os.environ.get("WING_TOOL_KEY"),
        help="Tool_runtime API key the container uses (env: WING_TOOL_KEY)",
    )

    # Container
    run_parser.add_argument(
        "--client-id",
        default=None,
        help="Tool container namespace (default: auto-generated task-<hex>)",
    )
    run_parser.add_argument(
        "--image",
        default=None,
        help="Tool container image (default: $REGISTRY/wing-devbox:$TAG)",
    )
    run_parser.add_argument(
        "--workspace",
        default=os.environ.get("WING_WORKSPACE"),
        help="Workspace path (default: $WING_WORKSPACE)",
    )
    run_parser.add_argument(
        "--network",
        default=None,
        help="Docker network (default: share the parent's network stack)",
    )

    # Agent
    run_parser.add_argument(
        "--model",
        default=os.environ.get("DEFAULT_MODEL", DEFAULT_MODEL),
        help=f"Executor model (default: {DEFAULT_MODEL})",
    )
    run_parser.add_argument(
        "--tools",
        default=None,
        help="Comma-separated tool refs (default: <client_id>.<standard tools>)",
    )
    run_parser.add_argument(
        "--append-system-prompt",
        default=None,
        help="Extra text to append to the executor system prompt",
    )
    run_parser.add_argument(
        "--timeout",
        type=float,
        default=1800.0,
        help="Seconds to wait for a result (default: 1800)",
    )
    run_parser.add_argument(
        "--keep",
        action="store_true",
        help="Keep the tool container after the run (default: remove it)",
    )
    run_parser.add_argument("--log-level", default="INFO", help="Log level")

    # ── cleanup ───────────────────────────────────────────────
    sub.add_parser("cleanup", help="Remove all leftover wing-spawn containers")

    # Backwards-compatible: bare `wing-spawn "prompt"` / `wing-spawn --flag …`
    # without a subcommand defaults to `run`.
    argv = sys.argv[1:]
    if argv and argv[0] not in ("run", "cleanup"):
        argv = ["run", *argv]

    args = parser.parse_args(argv)

    if args.command == "cleanup":
        _cleanup()
        return

    if args.command == "run":
        _run(parser, args)
        return

    parser.print_help()


def _cleanup() -> None:
    from wing_spawn.containers import cleanup_all

    n = cleanup_all()
    print(f"Removed {n} leftover wing-spawn container(s).")


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
    )

    try:
        code = asyncio.run(runner.run())
    except KeyboardInterrupt:
        code = 130
    sys.exit(code)


if __name__ == "__main__":
    main()