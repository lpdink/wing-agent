"""devbox 工具宿主入口——注册六个标准工具，断线自动重连。"""

from __future__ import annotations

import asyncio
import logging
import os

from wing_sdk.host import ConnectionClosed, ToolHost
from wing_sdk.tools import register_standard_tools

log = logging.getLogger("toolhost")

RECONNECT_MAX_BACKOFF = 30.0


async def main() -> None:
    client_id = os.environ.get("TOOL_CLIENT_ID", "devbox")
    gateway_url = os.environ.get("WING_GATEWAY_URL", "http://gateway:32523")
    api_key = os.environ.get("WING_API_KEY") or None
    workspace = os.environ.get("WING_WORKSPACE", "/workspace/wing-agent")

    host = ToolHost(client_id=client_id, gateway_url=gateway_url, api_key=api_key)
    register_standard_tools(host, workspace)
    log.info(
        f"tool host '{client_id}' ready: {len(host.specs)} tools, "
        f"workspace={workspace}, gateway={gateway_url}"
    )

    backoff = 1.0
    while True:
        try:
            await host.run()
        except ConnectionClosed as e:
            log.warning(f"disconnected: {e}")
        except Exception as e:
            log.warning(f"tool host error: {e}")
        await asyncio.sleep(backoff)
        backoff = min(backoff * 2, RECONNECT_MAX_BACKOFF)


if __name__ == "__main__":
    logging.basicConfig(
        level=os.environ.get("LOG_LEVEL", "INFO"),
        format="%(asctime)s %(levelname)s [%(name)s] %(message)s",
    )
    asyncio.run(main())
