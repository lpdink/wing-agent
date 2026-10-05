# wing/gateway/cli.py — Gateway 命令行入口

"""
Gateway CLI——启动 WebSocket 服务器。

用法：
  wing-gateway                       # 默认 127.0.0.1:32523
  wing-gateway -p 8080               # 指定端口
  wing-gateway -H 127.0.0.1 -p 8080  # 指定 host + port

鉴权（api_key / tls）与守护进程模式由 config.yaml 与 Rust 侧 `wing start` 提供，
本入口只做 host / port 覆盖与进程内启动。
"""

from __future__ import annotations

import argparse
import sys

from wing.common.logger import setup_logger
from wing.config import get_config
from wing.commands import register_prompt_commands

from .server import GatewayServer


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="wing-gateway",
        description="Wing Gateway — WebSocket 服务器",
    )
    parser.add_argument(
        "-H",
        "--host",
        default=None,
        help="监听地址（默认从 config.yaml 读取，127.0.0.1）",
    )
    parser.add_argument(
        "-p",
        "--port",
        type=int,
        default=None,
        help="监听端口（默认从 config.yaml 读取，32523）",
    )
    args = parser.parse_args()

    config = get_config()

    # 初始化日志（控制台级别来自 config；文件日志见 common/logger.py 策略）。
    # 库代码 import 时不再有日志副作用，网关进程在此显式挂载 handler。
    setup_logger(level=config.log.level)

    register_prompt_commands(config.commands.paths)

    # CLI args override config; config provides defaults
    host = args.host if args.host is not None else config.gateway.host
    port = args.port if args.port is not None else config.gateway.port

    server = GatewayServer(host=host, port=port)
    try:
        server.start()
    except KeyboardInterrupt:
        print("\nGateway 已停止")
        sys.exit(0)


if __name__ == "__main__":
    main()
