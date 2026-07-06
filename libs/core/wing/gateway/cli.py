# wing_gateway/cli.py — Gateway 命令行入口

"""
Gateway CLI——启动 WebSocket 服务器。

用法：
  wing-gateway                    # 默认 127.0.0.1:32523
  wing-gateway -p 8080            # 指定端口
  wing-gateway -H 127.0.0.1 -p 8080  # 指定 host + port

V2 升级路径：
  - 加 --key / --credentials-file 参数
  - 加 --tls-psk 参数
  - 加 --daemon 参数（守护进程模式）
"""

from __future__ import annotations

import argparse
import sys

from wing.config import get_config
from wing.magic_command import register_prompt_commands

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

    # 加载配置
    config = get_config()
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
