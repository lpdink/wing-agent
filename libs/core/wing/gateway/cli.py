# wing/gateway/cli.py — Gateway 命令行入口

"""
Gateway CLI——启动 WebSocket 服务器。

用法：
  wing-gateway                       # 默认 127.0.0.1:32523
  wing-gateway -p 8080               # 指定端口
  wing-gateway -H 127.0.0.1 -p 8080  # 指定 host + port

鉴权（api_key）与守护进程模式分别由 config.yaml 与 Rust 侧 `wing start` 提供
（TLS 交给反向代理）；本入口只做 host / port 覆盖与进程内启动。

**降级启动（04）**：配置缺失 / 非法不再让进程退出——`boot_config()` 永不抛，
配置不可用时打印修复横幅并让网关以 **setup mode** 起来（只服务设置端点，其余 503），
保存出合法配置后由 `GatewayServer` 就地转入正常模式（不重启进程）。
"""

from __future__ import annotations

import argparse
import sys

from wing.common.logger import setup_logger
from wing.config.boot import boot_config
from wing.commands import register_prompt_commands
from wing.request_context import get_request_context

from .server import DEFAULT_HOST, DEFAULT_PORT, GatewayServer


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

    # 启动读取**永不抛**：配置非法 ⇒ 降级启动（setup mode），而不是进程退出。
    boot = boot_config()
    config = boot.config if boot.ok else None

    # 初始化日志（控制台级别来自 config；文件日志见 common/logger.py 策略）。
    # 库代码 import 时不再有日志副作用，网关进程在此显式挂载 handler。
    # 关联上下文由组合根接线：formatter 逐条从 get_request_context() 读
    # session / request id（common 是 L0，不能反向 import request_context）。
    # 配置不可用时用 WARNING：降级启动的横幅与 boot 问题清单必须在终端看得见。
    setup_logger(
        level=config.log.level if config is not None else "WARNING",
        context=get_request_context,
    )

    if config is not None:
        # 配置不可用时不注册——转入正常模式时由 _enter_operational 的 ③ 补上。
        register_prompt_commands(config.commands.paths)

    # CLI args override config; config provides defaults.
    # 配置语义不合法（但文件能解析）时仍取文件里的 gateway.host/port——
    # Rust 侧 backend_config.rs 读同一份文件的同一段，两边必须落在同一个 endpoint 上
    # （否则 TUI 连不上降级启动的网关，总设计 §8.6）；取不到才回落默认。
    file_host, file_port = boot.endpoint or (DEFAULT_HOST, DEFAULT_PORT)
    gateway = config.gateway if config is not None else None
    host = (
        args.host
        if args.host is not None
        else (gateway.host if gateway is not None else file_host)
    )
    port = (
        args.port
        if args.port is not None
        else (gateway.port if gateway is not None else file_port)
    )

    server = GatewayServer(host=host, port=port, boot=boot)
    if server.in_setup_mode:
        # 降级横幅：**构造之后**按 server 的真实模式打一次，覆盖两条路径——
        # ① boot 失败（配置缺失 / 语法错 / 校验不过）；② 配置合法但运行时装配失败
        # （``_enter_operational`` 失败 ⇒ ``_record_startup_failure``，AD14 的数据面
        # 已补齐，这里补上终端这条最显眼的通道）。问题清单复用 server 的快照
        # （② 的那批只存在于 server 上），且只打一次（这里不再有构造前的重复打印）。
        reason = (
            server.boot_reason.value if server.boot_reason is not None else "unknown"
        )
        # flush=True：启动信息必须立刻可见——无论 stdout 是终端、管道还是被
        # `wing start` 捕获（块缓冲会把横幅扣在缓冲区里，而它正是「怎么修」的唯一提示）。
        print(f"⚠ 配置不可用（{reason}）：网关以修复模式启动 {host}:{port}", flush=True)
        for problem in server.setup_problems[:10]:
            print(f"   · {problem.path or '<document>'}: {problem.message}", flush=True)
        print(
            "   运行 `wing` 打开设置面板修复，或 `wing config doctor` 查看详情。",
            flush=True,
        )

    try:
        server.start()
    except KeyboardInterrupt:
        print("\nGateway 已停止")
        sys.exit(0)


if __name__ == "__main__":
    main()
