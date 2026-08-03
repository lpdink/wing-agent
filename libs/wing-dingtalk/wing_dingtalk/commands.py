"""前端命令——`/new` `/model` `/restart` `/help` `/sessions` `/switch` `/interrupt`。

未匹配的 `/xxx` 原样转发给 Gateway（后端魔术命令，如 /plan）。
"""

from __future__ import annotations

import asyncio
import logging

from .link import GatewayLink
from .models_match import match_model
from .router import Conversation

log = logging.getLogger("wing-dingtalk.commands")

# (命令, 参数说明, 描述) —— /help 展开用
FRONTEND_COMMANDS: list[tuple[str, str, str]] = [
    ("/new", "", "新建 session（当前会话切换过去）"),
    (
        "/model",
        "[名称 | provider:名称]",
        "切换模型。裸名跨 provider 模糊匹配；provider:名称 定向匹配；不带参数列出所有模型",
    ),
    ("/restart", "", "重启 Gateway（自动等待恢复并重新订阅）"),
    ("/sessions", "", "列出最近的 session"),
    ("/switch", "<session id 前缀>", "切换到一个已有 session"),
    ("/interrupt", "", "中断当前 session 的进行中任务"),
    ("/help", "", "本帮助"),
]

RESTART_TIMEOUT_S = 120.0


class CommandHandler:
    """前端命令分发与执行。"""

    def __init__(self, link: GatewayLink) -> None:
        self.link = link

    async def handle(self, conv: Conversation, text: str) -> bool:
        """尝试作为前端命令处理。返回 False 表示不是前端命令。"""
        if not text.startswith("/"):
            return False
        head, _, rest = text.partition(" ")
        cmd, rest = head.lower(), rest.strip()
        handlers = {
            "/new": self._new,
            "/model": self._model,
            "/restart": self._restart,
            "/sessions": self._sessions,
            "/switch": self._switch,
            "/interrupt": self._interrupt,
            "/help": self._help,
        }
        handler = handlers.get(cmd)
        if handler is None:
            return False
        try:
            await handler(conv, rest)
        except Exception as e:
            log.exception(f"command {cmd} failed")
            await self.link.notify(conv, f"❌ 命令执行失败: {e}")
        return True

    # ── 各命令 ────────────────────────────────────────────────

    async def _new(self, conv: Conversation, _: str) -> None:
        session_id = await self.link.new_session(conv)
        await self.link.notify(conv, f"🆕 新 session: `{session_id[:8]}`")

    async def _model(self, conv: Conversation, query: str) -> None:
        groups = await self.link.models()
        if not query:
            await self._model_list(conv, groups)
            return
        result = match_model(groups, query)
        if result.hit:
            if not conv.session_id:
                await self.link.ensure_session(conv)
            assert conv.session_id is not None
            await self.link.http.update_session(
                conv.session_id, model=result.hit.model, provider=result.hit.provider
            )
            await self.link.notify(
                conv, f"🔀 切换模型: {result.hit.provider}:{result.hit.model}"
            )
        elif result.candidates:
            lines = ["🤔 匹配到多个模型，请更精确一些:", ""]
            lines += [f"- {r}" for r in result.candidates[:20]]
            await self.link.notify(conv, "\n".join(lines))
        else:
            await self.link.notify(conv, f"❌ {result.error}")

    async def _model_list(self, conv: Conversation, groups: list[dict]) -> None:
        if not groups:
            await self.link.notify(conv, "（gateway 没有返回任何模型）")
            return
        lines = ["📋 可用模型:", ""]
        for group in groups:
            provider = group.get("provider", "?")
            models = group.get("models", [])
            lines.append(f"**{provider}** ({len(models)})")
            for m in models[:30]:
                lines.append(f"- {m}")
            if len(models) > 30:
                lines.append(f"- … 另有 {len(models) - 30} 个")
            lines.append("")
        lines.append("用法: /model <名称> 或 /model <provider>:<名称>")
        await self.link.notify_markdown(conv, "\n".join(lines))

    async def _restart(self, conv: Conversation, _: str) -> None:
        await self.link.notify(conv, "🔄 正在重启 Gateway…")
        try:
            await self.link.http.shutdown()
        except Exception:
            pass  # 进程被杀时连接可能先断——预期内

        # 等 Gateway 回来（link 自身也在重连，这里只等 health）
        loop = asyncio.get_running_loop()
        deadline = loop.time() + RESTART_TIMEOUT_S
        while loop.time() < deadline:
            try:
                await self.link.health()
                break
            except Exception:
                await asyncio.sleep(1.5)
        else:
            await self.link.notify(conv, "❌ Gateway 未在时限内恢复")
            return

        # 等 link 完成重连 + 重新订阅
        try:
            await asyncio.wait_for(self.link.connected.wait(), timeout=30.0)
        except asyncio.TimeoutError:
            await self.link.notify(conv, "⚠️ Gateway 已恢复，但事件链路尚未就绪")
            return

        health = await self.link.health()
        await self.link.notify(
            conv,
            f"✅ Gateway 已重启（v{health.get('version', '?')}，"
            f"uptime {health.get('uptime', 0)}s）",
        )

    async def _sessions(self, conv: Conversation, _: str) -> None:
        sessions = await self.link.sessions()
        if not sessions:
            await self.link.notify(conv, "（没有 session）")
            return
        sessions.sort(key=lambda s: s.get("last_interaction") or "", reverse=True)
        lines = ["🗂 最近的 session:", ""]
        for s in sessions[:10]:
            mark = "👉 " if s.get("id") == conv.session_id else ""
            name = s.get("name") or "(未命名)"
            lines.append(
                f"- {mark}`{s['id'][:8]}` [{s.get('status', '?')}] {name[:40]}"
            )
        lines.append("")
        lines.append("切换: /switch <id 前缀>；新建: /new")
        await self.link.notify_markdown(conv, "\n".join(lines))

    async def _switch(self, conv: Conversation, id_prefix: str) -> None:
        if len(id_prefix) < 4:
            await self.link.notify(conv, "❌ 请提供至少 4 位的 session id 前缀")
            return
        sessions = await self.link.sessions()
        matched = [s for s in sessions if s.get("id", "").startswith(id_prefix)]
        if len(matched) == 0:
            # 允许 resume 未出现在列表中的旧 session（inactive 也在列表里，
            # 这里兜底直接试）
            try:
                await self.link.switch_session(conv, id_prefix)
                await self.link.notify(conv, f"🔁 已切换: `{id_prefix[:8]}`")
            except Exception as e:
                await self.link.notify(conv, f"❌ 找不到匹配的 session: {e}")
            return
        if len(matched) > 1:
            lines = ["🤔 前缀匹配到多个:", ""]
            lines += [f"- `{s['id'][:8]}` {s.get('name') or ''}" for s in matched]
            await self.link.notify(conv, "\n".join(lines))
            return
        target = matched[0]["id"]
        await self.link.switch_session(conv, target)
        await self.link.notify(conv, f"🔁 已切换: `{target[:8]}`")

    async def _interrupt(self, conv: Conversation, _: str) -> None:
        if not conv.session_id:
            await self.link.notify(conv, "（当前会话没有绑定 session）")
            return
        await self.link.http.interrupt_session(conv.session_id)
        await self.link.notify(conv, "⏹ 已发送中断请求。")

    async def _help(self, conv: Conversation, _: str) -> None:
        lines = ["## 🤖 Wing 钉钉助理", ""]

        try:
            health = await self.link.health()
            lines.append(
                f"Gateway v{health.get('version', '?')} · "
                f"uptime {health.get('uptime', 0)}s"
            )
        except Exception:
            lines.append("Gateway: ⚠️ 不可达")
        if conv.session_id:
            try:
                info = await self.link.http.get_session_info(conv.session_id)
                lines.append(
                    f"当前 session: `{conv.session_id[:8]}` · "
                    f"模型: {info.get('model', '?')}"
                )
            except Exception:
                lines.append(f"当前 session: `{conv.session_id[:8]}`")
        try:
            agents = await self.link.agents()
            lines.append(
                f"Agent 模板: {', '.join(agents.get('agents', []))}"
                f"（默认 {agents.get('default_agent', '?')}）"
            )
        except Exception:
            pass

        lines += ["", "### 前端命令", ""]
        for name, params, desc in FRONTEND_COMMANDS:
            usage = f"{name} {params}".strip()
            lines.append(f"- `{usage}` — {desc}")

        try:
            commands = await self.link.commands()
            if commands:
                lines += ["", "### 后端魔术命令", ""]
                for c in commands:
                    aliases = ", ".join(f"/{a}" for a in c.get("aliases", []))
                    alias_part = f"（{aliases}）" if aliases else ""
                    lines.append(
                        f"- `/{c['name']}` {c.get('params', '')} — "
                        f"{c.get('description', '')}{alias_part}".rstrip()
                    )
                lines += ["", "魔术命令直接发送即可（如 `/plan ...`）"]
        except Exception:
            pass

        await self.link.notify_markdown(conv, "\n".join(lines))
