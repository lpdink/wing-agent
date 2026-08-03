"""GatewayLink——与 Wing Gateway 的 WS 事件链路 + HTTP 操作。

职责：
- WS 订阅所有已绑定 session 的事件（断线自动重连 + 重新 resume/subscribe）。
- 事件 → 钉钉消息：turn_result（最终结果）、ask（逐题问答）、error、
  interrupted、session_state_changed（模型切换确认）。
- 提供给命令层使用的 HTTP 操作（建 session、切模型、重启等）。

参考实现：wing-orch 的 runner（同为 wing-sdk 事件消费模式）。
"""

from __future__ import annotations

import asyncio
import json
import logging
from typing import Any
from urllib.parse import quote

import httpx
import websockets

from wing_sdk.host import WS_MAX_SIZE
from wing_sdk.http_client import GatewayClient

from .config import Config
from .router import Conversation, PendingAsk, Router
from .sender import DingSender

log = logging.getLogger("wing-dingtalk.link")

RECONNECT_MAX_BACKOFF = 30.0


class GatewayLink:
    """Gateway 事件链路 + 操作入口。"""

    def __init__(self, cfg: Config, router: Router, sender: DingSender) -> None:
        self.cfg = cfg
        self.router = router
        self.sender = sender
        self.http = GatewayClient(cfg.gateway_url, cfg.api_key)
        self.connected = asyncio.Event()

    async def close(self) -> None:
        await self.http.close()

    # ── 事件链路（常驻） ──────────────────────────────────────

    async def run(self) -> None:
        """重连循环——Gateway 重启 / 网络抖动后自动恢复。"""
        backoff = 1.0
        while True:
            try:
                await self._wait_gateway()
                await self._run_once()
                backoff = 1.0  # 正常结束（不太会发生）也重置退避
            except asyncio.CancelledError:
                raise
            except Exception as e:
                log.warning(f"gateway link down: {e}")
            self.connected.clear()
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, RECONNECT_MAX_BACKOFF)

    async def _wait_gateway(self) -> None:
        """轮询 /api/health 直到 Gateway 可用（启动顺序 / 重启场景）。"""
        while True:
            try:
                await self.http.health()
                return
            except Exception:
                await asyncio.sleep(1.5)

    async def _run_once(self) -> None:
        ws_url = self.cfg.gateway_url.replace("http://", "ws://").replace(
            "https://", "wss://"
        )
        headers: dict[str, str] = {}
        if self.cfg.api_key:
            headers["Authorization"] = f"Bearer {self.cfg.api_key}"
        uri = f"{ws_url}/ws?client_id={quote(self.cfg.event_client_id, safe='')}"

        async with websockets.connect(
            uri, additional_headers=headers, max_size=WS_MAX_SIZE
        ) as ws:
            first = json.loads(await ws.recv())
            if first.get("type") != "connected":
                raise RuntimeError(f"unexpected first frame: {first}")
            log.info(f"ws connected as {self.cfg.event_client_id}")

            await self._wait_tools_and_reload()
            await self._restore_subscriptions()
            self.connected.set()

            async for raw in ws:
                try:
                    event = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                await self._dispatch(event)

    async def _wait_tools_and_reload(self) -> None:
        """等待工具宿主注册完成，然后 reload 配置重建 agent 模板。

        Gateway 启动早于工具宿主，模板首次解析会静默丢弃未注册的远程
        工具引用；宿主就位后 reload 一次，模板即带上完整工具集。
        每次（重）连接都执行——Gateway 重启后同样存在该窗口。
        """
        expected = set(self.cfg.expected_tool_refs)
        expected.add(f"{self.cfg.tool_client_id}.SendFile")

        loop = asyncio.get_running_loop()
        deadline = loop.time() + self.cfg.expected_tools_timeout_s
        while loop.time() < deadline:
            try:
                resp = await self.http.list_tools()
                refs = {t.get("ref", "") for t in resp.get("tools", [])}
                missing = expected - refs
                if not missing:
                    break
                log.info(f"waiting for tool hosts: {sorted(missing)}")
            except Exception:
                pass
            await asyncio.sleep(2.0)
        else:
            log.warning(
                f"tool hosts not fully registered within "
                f"{self.cfg.expected_tools_timeout_s:.0f}s, reloading anyway"
            )

        try:
            result = await self.http.reload()
            if not result.get("ok", False):
                log.warning(f"reload reported failure: {result}")
            else:
                log.info("config reloaded — agent templates rebuilt")
        except Exception as e:
            log.warning(f"reload failed: {e}")

    async def _restore_subscriptions(self) -> None:
        """重连后恢复所有已绑定 session：resume（尽力）+ subscribe。"""
        for session_id in self.router.all_session_ids():
            try:
                await self.http.resume_session(session_id)
            except httpx.HTTPStatusError as e:
                if e.response.status_code != 404:
                    log.warning(f"resume {session_id} failed: {e}")
            except Exception as e:
                log.warning(f"resume {session_id} failed: {e}")
            try:
                await self.http.subscribe(session_id, self.cfg.event_client_id)
            except Exception as e:
                log.warning(f"subscribe {session_id} failed: {e}")

    # ── 事件分发 ──────────────────────────────────────────────

    async def _dispatch(self, event: dict) -> None:
        etype = event.get("type", "")
        session_id = event.get("session_id", "") or ""
        conv = self.router.conversation_for_session(session_id)

        if etype == "turn_result":
            await self._on_turn_result(conv, event)
        elif etype == "ask":
            await self._on_ask(conv, session_id, event)
        elif etype == "error":
            await self._on_error(conv, event)
        elif etype == "interrupted":
            if conv is not None:
                await self.notify(conv, "⏹️ 已中断。")
        elif etype == "session_state_changed":
            model = event.get("model")
            if model and conv:
                await self._safe_send_text(conv, f"🔀 当前模型: {model}")

    async def _on_turn_result(self, conv: Conversation | None, event: dict) -> None:
        if conv is None:
            return
        result = event.get("result") or ""
        errors = event.get("errors") or []
        is_error = bool(event.get("is_error"))
        text = result.strip()
        if is_error:
            prefix = "⚠️ 本轮执行出错"
            if errors:
                prefix += f"（{'; '.join(str(e) for e in errors[:3])}）"
            text = f"{prefix}\n\n{text}" if text else prefix
        if not text:
            text = f"✅ 完成（{event.get('num_turns', 0)} 轮）"
        await self._safe_send_markdown(conv, text)

    async def _on_ask(
        self, conv: Conversation | None, session_id: str, event: dict
    ) -> None:
        if conv is None:
            return
        questions = event.get("questions") or []
        if not questions:
            # 旧式单问题格式（Bash 危险命令确认）
            question = event.get("question") or ""
            if not question:
                return
            questions = [
                {
                    "id": "answer",
                    "question": question,
                    "choices": event.get("choices") or [],
                }
            ]
        conv.pending_ask = PendingAsk(
            tool_call_id=event.get("tool_call_id", ""),
            session_id=session_id,
            questions=questions,
        )
        self.router.save()
        await self._safe_send_text(conv, self._render_question(conv))

    def _render_question(self, conv: Conversation) -> str:
        ask = conv.pending_ask
        assert ask is not None
        q = ask.current()
        assert q is not None
        lines = [f"❓ Agent 提问（{ask.progress()}）", "", str(q.get("question", ""))]
        choices = q.get("choices") or []
        if choices:
            lines.append("")
            lines.append("选项: " + " / ".join(str(c) for c in choices))
            lines.append("（回复选项或任意文本均可）")
        return "\n".join(lines)

    async def handle_ask_answer(self, conv: Conversation, answer: str) -> None:
        """用户消息作为 Ask 应答——推进问答流程，收齐后回传后端。"""
        ask = conv.pending_ask
        assert ask is not None
        ask.answers.append(answer)
        ask.idx += 1
        if ask.current() is not None:
            self.router.save()
            await self._safe_send_text(conv, self._render_question(conv))
            return

        payload = json.dumps(
            {
                q.get("id", f"q{i}"): a
                for i, (q, a) in enumerate(zip(ask.questions, ask.answers))
            },
            ensure_ascii=False,
        )
        conv.pending_ask = None
        self.router.save()
        try:
            await self.http.send_message(
                ask.session_id, payload, tool_call_id=ask.tool_call_id
            )
            await self._safe_send_text(conv, "📨 已转达 Agent。")
        except Exception as e:
            await self._safe_send_text(conv, f"❌ 应答提交失败: {e}")

    async def _on_error(self, conv: Conversation | None, event: dict) -> None:
        message = event.get("message") or "unknown error"
        detail = event.get("detail") or ""
        text = f"❌ {message}" + (f"\n{detail}" if detail else "")
        if conv is not None:
            await self._safe_send_text(conv, text)
        else:
            log.error(f"unrouted error event: {text}")

    # ── 操作（命令层调用） ────────────────────────────────────

    async def _await_ready(self) -> None:
        """等待链路就绪（工具宿主就位 + 模板重建 + 订阅恢复）。"""
        try:
            await asyncio.wait_for(
                self.connected.wait(),
                timeout=self.cfg.expected_tools_timeout_s + 30.0,
            )
        except asyncio.TimeoutError:
            log.warning("gateway link not ready in time, proceeding anyway")

    async def ensure_session(self, conv: Conversation) -> str:
        """会话无 session 时自动创建并绑定。返回 session_id。"""
        if conv.session_id:
            return conv.session_id
        await self._await_ready()
        resp = await self.http.create_session(workspace=self.cfg.workspace)
        session_id = resp["session_id"]
        self.router.set_session(conv, session_id)
        await self._subscribe_quiet(session_id)
        log.info(f"created session {session_id} for {conv.address}")
        return session_id

    async def new_session(self, conv: Conversation) -> str:
        await self._await_ready()
        resp = await self.http.create_session(workspace=self.cfg.workspace)
        session_id = resp["session_id"]
        self.router.set_session(conv, session_id)
        await self._subscribe_quiet(session_id)
        return session_id

    async def switch_session(self, conv: Conversation, session_id: str) -> None:
        await self.http.resume_session(session_id)
        self.router.set_session(conv, session_id)
        await self._subscribe_quiet(session_id)

    async def send_user_message(self, conv: Conversation, content: str) -> None:
        session_id = await self.ensure_session(conv)
        await self.http.send_message(session_id, content)

    async def _subscribe_quiet(self, session_id: str) -> None:
        try:
            await self.http.subscribe(session_id, self.cfg.event_client_id)
        except Exception as e:
            log.warning(f"subscribe {session_id} failed: {e}")

    # ── 发送兜底（发送失败不炸事件循环） ──────────────────────

    async def _safe_send_text(self, conv: Conversation, text: str) -> None:
        try:
            await self.sender.send_text(conv, text[: self.cfg.max_message_chars])
        except Exception as e:
            log.error(f"send_text failed ({conv.address}): {e}")

    async def _safe_send_markdown(self, conv: Conversation, text: str) -> None:
        text = text[: self.cfg.max_message_chars]
        try:
            await self.sender.send_markdown(conv, "Wing", text)
        except Exception as e:
            log.error(f"send_markdown failed ({conv.address}): {e}")

    # 供命令层使用的统一通知入口
    async def notify(self, conv: Conversation, text: str) -> None:
        await self._safe_send_text(conv, text)

    async def notify_markdown(self, conv: Conversation, text: str) -> None:
        await self._safe_send_markdown(conv, text)

    # ── 查询（help 用） ──────────────────────────────────────

    async def health(self) -> dict[str, Any]:
        return await self.http.health()

    async def commands(self) -> list[dict]:
        resp = await self.http.get_commands()
        return resp.get("commands", [])

    async def agents(self) -> dict[str, Any]:
        return await self.http.get_agents()

    async def models(self) -> list[dict]:
        resp = await self.http.get_models()
        return resp.get("providers", [])

    async def sessions(self) -> list[dict]:
        resp = await self.http.list_sessions()
        return resp.get("sessions", [])
