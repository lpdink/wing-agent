"""Bot——钉钉 Stream 接入 + 消息分流编排。

收消息 → 白名单 → emoji 回执 → 分流：
  Ask 应答中     → 作为答案推进问答流程
  /前端命令      → CommandHandler
  其余（含后端魔术命令）→ 注入昵称后转发 Gateway
"""

from __future__ import annotations

import asyncio
import json
import logging

from dingtalk_stream import ChatbotMessage, Credential, DingTalkStreamClient
from dingtalk_stream.frames import AckMessage, CallbackMessage
from dingtalk_stream.handlers import CallbackHandler

from wing_sdk.host import ConnectionClosed

from .commands import CommandHandler
from .config import Config
from .file_tool import build_file_host
from .link import GatewayLink
from .router import Conversation, Router
from .sender import DingSender

log = logging.getLogger("wing-dingtalk.bot")


class Bot:
    """钉钉前端主体——持有所有组件并驱动事件循环。"""

    def __init__(self, cfg: Config) -> None:
        self.cfg = cfg
        self.router = Router(cfg.state_dir / "router.json")
        self.sender = DingSender(cfg.dingtalk_client_id, cfg.dingtalk_client_secret)
        self.link = GatewayLink(cfg, self.router, self.sender)
        self.commands = CommandHandler(self.link)

        credential = Credential(cfg.dingtalk_client_id, cfg.dingtalk_client_secret)
        self.dt_client = DingTalkStreamClient(credential)
        self.dt_client.register_callback_handler(
            ChatbotMessage.TOPIC, _MessageHandler(self)
        )

    async def run(self) -> None:
        self.router.load()
        if not self.cfg.allowed_users:
            log.warning("DINGTALK_ALLOWED_USERS is empty — allowing EVERYONE")

        tasks = [
            asyncio.create_task(self.dt_client.start(), name="dingtalk-stream"),
            asyncio.create_task(self.link.run(), name="gateway-link"),
            asyncio.create_task(self._tool_host_loop(), name="tool-host"),
        ]
        try:
            await asyncio.gather(*tasks)
        finally:
            await self.link.close()
            await self.sender.close()

    async def _tool_host_loop(self) -> None:
        """SendFile 工具宿主——断线自动重连（Gateway 重启场景）。"""
        host = build_file_host(self.cfg, self.router, self.sender)
        backoff = 1.0
        while True:
            try:
                await host.run()
            except asyncio.CancelledError:
                raise
            except ConnectionClosed as e:
                log.warning(f"tool host disconnected: {e}")
            except Exception as e:
                log.warning(f"tool host error: {e}")
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 30.0)

    # ── 入站消息 ──────────────────────────────────────────────

    async def on_message(self, data: dict) -> None:
        try:
            await self._handle(data)
        except Exception:
            log.exception(
                f"failed to handle message: {json.dumps(data, ensure_ascii=False)[:500]}"
            )

    async def _handle(self, data: dict) -> None:
        conv = self.router.upsert_from_message(data)

        if not self._allowed(data):
            await self.sender.send_text(
                conv,
                f"⛔ 您不在白名单内（staffId: {data.get('senderStaffId', '?')}）。",
            )
            return

        self.router.save()

        if data.get("msgtype") != "text":
            await self.sender.send_text(conv, "目前只支持文本消息。")
            return

        text = ((data.get("text") or {}).get("content") or "").strip()
        if not text:
            return

        # 收到即回执（emoji）
        await self.sender.send_text(conv, self.cfg.ack_emoji)

        # Ask 应答优先于普通消息；但前端命令仍可插入（如 /interrupt）
        if text.startswith("/"):
            if await self.commands.handle(conv, text):
                return
            if conv.pending_ask is not None:
                await self.link.handle_ask_answer(conv, text)
                return
            # 后端魔术命令（/plan 等）——注入昵称后转发
            await self.link.send_user_message(conv, self._with_nick(conv, text))
            return

        if conv.pending_ask is not None:
            await self.link.handle_ask_answer(conv, text)
            return

        await self.link.send_user_message(conv, self._with_nick(conv, text))

    @staticmethod
    def _with_nick(conv: Conversation, text: str) -> str:
        """注入发送者昵称——多人场景下 Agent 能识别是谁在说话。"""
        if conv.nick:
            return f"[{conv.nick}] {text}"
        return text

    def _allowed(self, data: dict) -> bool:
        if not self.cfg.allowed_users:
            return True
        candidates = {
            str(data.get("senderStaffId") or ""),
            str(data.get("senderId") or ""),
            str(data.get("senderNick") or "").strip(),
        }
        return any(user in candidates for user in self.cfg.allowed_users)


class _MessageHandler(CallbackHandler):
    """钉钉回调 handler——立即 ack，消息交给 Bot 异步处理。"""

    def __init__(self, bot: Bot) -> None:
        super().__init__()
        self._bot = bot

    async def process(self, message: CallbackMessage) -> tuple[int, str]:
        data = message.data
        if isinstance(data, str):
            try:
                data = json.loads(data)
            except json.JSONDecodeError:
                return AckMessage.STATUS_OK, "bad payload"
        asyncio.create_task(self._bot.on_message(data))
        return AckMessage.STATUS_OK, "OK"
