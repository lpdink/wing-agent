"""CLI 入口——`wing-dingtalk`。"""

from __future__ import annotations

import asyncio
import logging
import signal

from .bot import Bot
from .config import Config

LOG_FORMAT = "%(asctime)s %(levelname)s [%(name)s] %(message)s"


def main() -> None:
    cfg = Config.from_env()
    logging.basicConfig(level=cfg.log_level.upper(), format=LOG_FORMAT)
    # dingtalk-stream 内部日志随全局 level
    cfg.validate()

    bot = Bot(cfg)

    async def _run() -> None:
        loop = asyncio.get_running_loop()
        main_task = asyncio.current_task()
        assert main_task is not None
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, main_task.cancel)
        try:
            await bot.run()
        except asyncio.CancelledError:
            logging.getLogger("wing-dingtalk").info("shutting down")

    try:
        asyncio.run(_run())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
