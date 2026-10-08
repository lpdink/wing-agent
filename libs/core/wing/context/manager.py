# wing/context/manager.py
import asyncio
import json
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING

from wing.chain import TrackedList
from wing.common.logger import log
from wing.provider.base import ModelProvider
from wing.schema import (
    AgentSkill,
    ChainNode,
    LLMUsage,
    Message,
    Tool,
)

from .compaction import Compactor, LLMMessagesResult, PendingCompact
from .resources import (
    build_skills_prompt,
    load_all_skills,
    load_rules,
    render_skills_info,
)

if TYPE_CHECKING:
    from wing.event import WingEvent
    from wing.provider.base import RequestOptions


class ContextManager:
    def __init__(
        self,
        session_id: str,
        messages: TrackedList[ChainNode],
        system_prompt: str,
        compactor: Compactor,
        skills_patterns: list[str] | None = None,
        rules_patterns: list[str] | None = None,
        workspace: str | None = None,
    ) -> None:
        """
        Args:
            session_id: Session 标识符（只读，不用于生成或路径计算）
            messages: TrackedList 混合链（Message + 事件节点，外部创建）
            system_prompt: 系统提示词
            compactor: 压缩助手（必填）
            skills_patterns: Skills glob 模式列表
            rules_patterns: Rules glob 模式列表
            workspace: 工作目录，相对路径的 patterns 基于此解析
        """
        self.compactor = compactor
        self.setin_system_prompt = system_prompt

        self._session_id = session_id
        self._messages = messages
        self._workspace = Path(workspace).resolve() if workspace else None

        self._skills_patterns = skills_patterns or []
        self._rules_patterns = rules_patterns or []

        # _rules_files：实际匹配并成功读取的规则文件路径（load_rules 的返回）
        self._rules_prompt, self._rules_files = load_rules(
            self._rules_patterns, self._workspace
        )
        self._skills_cache: dict[str, AgentSkill] = load_all_skills(
            self._skills_patterns, self._workspace
        )
        self._skills_prompt = build_skills_prompt(self._skills_cache)
        # 会话级持久状态（metadata.append_system_prompt）：Session 构造时恢复、
        # create/update 后落盘——丢失会让系统提示词变化，碎掉 KV cache 前缀。
        self.append_system_prompt: str = ""

        # ── 异步 compact 状态 ──────────────────────
        self._pending_compact_task: asyncio.Task[None] | None = None
        self._pending_compact_result: PendingCompact | None = None
        self._pending_compact_result = self._load_pending_compact()

        # ── 工具声明集（LLM 可见视图）──────────────
        # 由 on_tools_changed() 管理。冻结策略：链非空时不动（保护 KV prefix cache），
        # compaction 时自动同步（cache 已碎）。Agent 不持有此状态。
        self._declared_tools: list[Tool] = []
        # 显式标志区分「未初始化」和「合法为空」。声明集**有意不持久化**：
        # resume/重建 agent 时新 CM 走 init 路径（_declared_initialized=False）
        # 直接采用当前可执行集，不会误触热切换注入 reminder。代价是「热切换
        # 工具后又重启」会改变 tools 声明（前缀碎裂一次）——远程工具与动态
        # 工具切换尚无系统化设计，先按最简单语义处理（已知限制，见
        # docs/dev/architecture.md）。
        self._declared_initialized: bool = False

    @property
    def id(self) -> str:
        """当前 session 的标识符。"""
        return self._session_id

    @property
    def skills_patterns(self) -> list[str]:
        """Skills glob 模式列表（构造时固化；副本，外部改动不回写）。"""
        return list(self._skills_patterns)

    @property
    def rules_patterns(self) -> list[str]:
        """Rules glob 模式列表（构造时固化；副本，外部改动不回写）。"""
        return list(self._rules_patterns)

    @property
    def workspace(self) -> Path | None:
        """当前工作目录（相对 patterns 的解析基准；None = 未设置）。"""
        return self._workspace

    def set_workspace(self, workspace: Path | None) -> None:
        """替换工作目录解析基准（唯一入口）。

        只替换路径字段——已缓存的 rules/skills 不重载（与既有语义一致：
        改 workspace 后需重建会话才重新加载）。
        """
        self._workspace = workspace

    @property
    def system_prompt(self) -> Message:
        """构造完整系统提示词，按顺序拼接：system_prompt + append + rules + skills"""
        parts = []
        if self.setin_system_prompt:
            parts.append(self.setin_system_prompt)
        if self.append_system_prompt:
            parts.append(self.append_system_prompt)
        if self._rules_prompt:
            parts.append(self._rules_prompt)
        if self._skills_prompt:
            parts.append(self._skills_prompt)
        return Message(role="system", content="\n\n".join(parts))

    def append_to_system_prompt(self, text: str) -> None:
        """追加系统提示词片段——append_system_prompt 的唯一写入入口。

        hook（如 workspace_env_inject 注入环境信息）与 AgentOverride
        共用本入口；多片段按追加顺序以换行连接，结果整体随会话持久化。
        空串忽略（None 语义的字符串形态）。

        约定：**写入端规范化（strip / 丢弃空串），还原端原样赋值**（Session
        直接把持久化值写回 `append_system_prompt`）——还原必须逐字节，不能再
        规整一次，否则落盘值与请求前缀会漂移。新增写入入口请同样走本方法。
        """
        stripped = text.strip()
        if not stripped:
            return
        if self.append_system_prompt:
            self.append_system_prompt = f"{self.append_system_prompt}\n{stripped}"
        else:
            self.append_system_prompt = stripped

    # ── 工具声明集管理 ─────────────────────────────

    @property
    def declared_tools(self) -> list[Tool]:
        """当前 LLM 可见工具集（冻结视图）。"""
        return list(self._declared_tools)

    def reset_declared_tools(self) -> None:
        """把声明集复位为「未初始化」——resume / fork 重建 agent 专用。

        声明集不持久化：重建后它必须**跟随可执行集**（on_tools_changed 的
        init 冷路径直接设置），而不是被当作热切换冻结并注入 System Reminder
        ——那会往重建后的链里塞一条重启前不存在的 reminder 消息，前缀与
        重启前不同（KV cache 碎裂）。
        """
        self._declared_initialized = False

    def on_tools_changed(self, new_tools: list[Tool]) -> None:
        """工具集变更通知——由 Agent.set_tools() 调用。

        策略：
        - 首次初始化（_declared_tools 为空）→ 直接设置（无论链状态）
        - 链为空（无 user/assistant 消息）→ 冷切换：直接更新声明集
        - 链非空 → 热切换：冻结声明集，注入 System Reminder

        Compaction 时声明集自动同步（见 get_messages_for_llm 内部）。
        """
        sorted_new = sorted(new_tools, key=lambda t: t.effective_llm_name)

        if not self._declared_initialized or self._chain_is_empty():
            self._declared_tools = sorted_new
            self._declared_initialized = True
        else:
            old_set = {
                (t.namespace, t.effective_llm_name) for t in self._declared_tools
            }
            new_set = {(t.namespace, t.effective_llm_name) for t in new_tools}
            removed = old_set - new_set
            added = new_set - old_set
            if removed or added:
                self._inject_tool_change_reminder(removed, added, new_tools)

    def _chain_is_empty(self) -> bool:
        """活跃链中是否存在 user 或 assistant 消息（事件节点不计）。"""
        for node in self._messages.active_chain:
            if isinstance(node, Message) and node.role in ("user", "assistant"):
                return False
        return True

    def _inject_tool_change_reminder(
        self,
        removed: set[tuple[str, str]],
        added: set[tuple[str, str]],
        current_tools: list[Tool],
    ) -> None:
        """向 context chain 追加 System Reminder，告知模型工具变更。"""
        lines: list[str] = [
            "[System Reminder] Your available tools have changed.",
            "",
        ]

        if removed:
            details = [f"{name} (namespace: {ns})" for ns, name in sorted(removed)]
            lines.append(f"Removed: {', '.join(details)}")

        if added:
            details = [f"{name} (namespace: {ns})" for ns, name in sorted(added)]
            lines.append(f"Added: {', '.join(details)}")

        lines.append("")
        lines.append("Available tools (full schema):")
        lines.append("<tools>")
        for tool in sorted(current_tools, key=lambda t: t.effective_llm_name):
            lines.append(json.dumps(tool.to_openai(), ensure_ascii=False))
        lines.append("</tools>")

        lines.append("")
        lines.append(
            "The tools listed in the system prompt above may be outdated.\n"
            "Use ONLY the tools listed in this reminder going forward.\n"
            "If you attempt to call a removed tool, you will receive an error."
        )

        self.add_message(Message(role="user", content="\n".join(lines)))

    def _sync_declared_tools(self, current_tools: list[Tool]) -> None:
        """Compaction 后同步声明集。"""
        self._declared_tools = sorted(current_tools, key=lambda t: t.effective_llm_name)

    def reload_skills_and_rules(self) -> None:
        """重新加载 skills 和 rules，用于 /reload 命令。"""
        self._rules_prompt, self._rules_files = load_rules(
            self._rules_patterns, self._workspace
        )
        self._skills_cache = load_all_skills(self._skills_patterns, self._workspace)
        self._skills_prompt = build_skills_prompt(self._skills_cache)

    def get_skills_info(self) -> str:
        """返回 skills/rules 信息，用于 /skills 命令显示。"""
        return render_skills_info(
            self._skills_patterns,
            self._skills_cache,
            self._rules_patterns,
            self._rules_files,
        )

    async def get_messages_for_llm(
        self,
        model: str,
        model_provider: ModelProvider,
        current_tools: Callable[[], list[Tool]],
        options: "RequestOptions | None" = None,
    ) -> LLMMessagesResult:
        """Get messages ready for LLM API call.

        异步 compact 三步流程（无同步 fallback）：
        1. 有 pending result + tokens >= context_window_tokens → 尝试 apply
        2. 有 running task → 等待（这次不 apply）
        3. tokens >= compact_window_tokens → 启动后台 compact task

        Returns:
            LLMMessagesResult(messages, tools)——tools 为本次调用应使用的声明集。

        Args:
            model: 主 model 名称（触发 compact 时透传给 provider）。
            model_provider: ModelProvider 实例（触发 compact 时使用；
                仅后台 compact 消费——主调用由调用方在发送时刻重新解析）。
            current_tools: 零参 callable，返回 Agent 当前可执行工具。
                compact sync 在 await 结束后求值，避免并发切换导致过期快照。
            options: 会话级调用参数（缓存亲和 session id / 开关）——compact
                请求与主调用保持同一 prompt cache key 与开关口径。
        """
        if not self.compactor:
            return LLMMessagesResult(
                [self.system_prompt] + self.get_context_window(),
                tools=list(self._declared_tools),
            )

        msgs = self.get_context_window()
        server_tokens = self._last_prompt_tokens()

        # ── Step 1: 有预计算结果？尝试 apply ──
        if self._pending_compact_result is not None:
            if self.compactor.need_apply_compact(msgs, server_tokens):
                if self._pending_compact_task and not self._pending_compact_task.done():
                    try:
                        await self._pending_compact_task
                    except Exception:
                        pass

                if self._pending_compact_result is not None:
                    end_idx = self._verify_snapshot_valid(msgs)
                    if end_idx is not None:
                        self._apply_pending_compact(msgs, end_idx)
                        # Compact 打破 prefix cache——同步声明集（await 后求值，避免过期快照）
                        self._sync_declared_tools(current_tools())
                        return LLMMessagesResult(
                            [self.system_prompt] + self.get_context_window(),
                            tools=list(self._declared_tools),
                        )
                    else:
                        self._discard_pending_compact()
            else:
                # 未到 apply 阈值：不能走 Step 2/3（避免重复启动 compact task）
                return LLMMessagesResult(
                    [self.system_prompt] + msgs, tools=list(self._declared_tools)
                )

        # ── Step 2: task 还在跑？ ──
        if self._pending_compact_task is not None:
            if self._pending_compact_task.done():
                self._pending_compact_task = None
            else:
                return LLMMessagesResult(
                    [self.system_prompt] + msgs, tools=list(self._declared_tools)
                )

        # ── Step 3: 该触发 early compact 了？ ──
        if self.compactor.need_early_trigger(msgs, server_tokens):
            self._start_background_compact(msgs, model, model_provider, options)

        return LLMMessagesResult(
            [self.system_prompt] + msgs, tools=list(self._declared_tools)
        )

    # ── 异步 compact 内部方法 ─────────────────────

    def _start_background_compact(
        self,
        msgs: list[Message],
        model: str,
        model_provider: ModelProvider,
        options: "RequestOptions | None" = None,
    ) -> None:
        """启动后台异步 compact task。"""
        preserve_last = msgs[-1].role == "user" if msgs else False
        head = msgs[:-1] if preserve_last else msgs
        cut_idx = self.compactor._calc_cut_idx(head)

        if cut_idx == 0:
            return

        start_uuid: str = head[0].uuid  # ty: ignore[invalid-assignment]
        end_uuid: str = head[cut_idx - 1].uuid  # ty: ignore[invalid-assignment]
        full_context = [self.system_prompt] + head[:cut_idx]
        # 使用声明集——与主调用 prefix 一致，最大化缓存命中
        compact_tools = list(self._declared_tools)

        async def _run() -> None:
            try:
                response = await self.compactor.do_compact(
                    full_context,
                    model,
                    model_provider,
                    tools=compact_tools,
                    options=options,
                )
                result = PendingCompact(
                    compact_content=response.content or "",
                    start_uuid=start_uuid,
                    end_uuid=end_uuid,
                    usage=response.usage,
                )
                self._pending_compact_result = result
                self._persist_pending_compact(result)
                log.info(f"Background compact done: {start_uuid[:8]}..{end_uuid[:8]}")
            except Exception as e:
                log.warning(f"Background compact failed: {e}")

        self._pending_compact_task = asyncio.create_task(_run())

    def _verify_snapshot_valid(self, msgs: list[Message]) -> int | None:
        """校验 pending compact 的 start/end UUID 仍在当前消息列表里。

        返回 compact 区间**末消息**的下标（apply 的插入点），UUID 对不上
        （rewind / 手动 compact 改过链）返回 None。start 也参与校验——区间
        必须完整存在且有序，但它不参与换入（摘要节点用 unzip_last_uuid
        编码区间，插入点由 end 决定）。
        """
        pc = self._pending_compact_result
        if pc is None:
            return None

        start_idx = None
        end_idx = None
        for i, msg in enumerate(msgs):
            if msg.uuid == pc.start_uuid:
                start_idx = i
            if msg.uuid == pc.end_uuid:
                end_idx = i

        if start_idx is not None and end_idx is not None and start_idx <= end_idx:
            return end_idx
        return None

    def _apply_pending_compact(self, msgs: list[Message], end_idx: int) -> None:
        """将预计算的 compact 结果换入消息链。

        摘要节点取代 msgs[:end_idx+1]（区间起点由它的 unzip_last_uuid
        编码），relink msgs[end_idx+1 :]。
        """
        pc = self._pending_compact_result
        if pc is None:
            raise RuntimeError("apply called without pending compact result")

        compact_node = Message(
            role="assistant",
            content=pc.compact_content,
            parent_uuid=None,
            unzip_last_uuid=pc.end_uuid,
        )
        compact_node.uuid = str(uuid.uuid4())

        tail = msgs[end_idx + 1 :]
        relink_tail: list[Message] = []
        prev_uuid: str | None = compact_node.uuid
        for msg in tail:
            relinked = Message(
                role=msg.role,
                content=msg.content,
                reasoning_content=msg.reasoning_content,
                content_blocks=msg.content_blocks,
                tool_calls=msg.tool_calls,
                tool_call_id=msg.tool_call_id,
                usage=msg.usage,
                parent_uuid=prev_uuid,
            )
            relinked.uuid = str(uuid.uuid4())
            relink_tail.append(relinked)
            prev_uuid = relinked.uuid

        # TODO(crash-safety): append_detached(compact_node) 和 set_tip 之间存在
        # 崩溃窗口。如果进程在 compact_node 写入后、relink tail set_tip 前被杀，
        # compact_node（parent_uuid=None）会成为新根，trace_chain 从它回溯到
        # None 就停了，原始 tail 消息从活跃链中丢失。这不是 async compact 引入
        # 的新问题——旧的同步 compact 也有同样的窗口。彻底修复需要 TrackedList
        # 支持批量原子写入（先写所有节点，再原子切换 tip）。
        self._messages.append_detached(compact_node)
        for msg in relink_tail:
            self._messages.append_detached(msg)

        if relink_tail:
            self._messages.set_tip(relink_tail[-1].uuid)  # ty: ignore[invalid-argument-type]
        else:
            self._messages.set_tip(compact_node.uuid)

        self._pending_compact_task = None
        self._pending_compact_result = None
        self._delete_pending_compact()

    def _discard_pending_compact(self) -> None:
        """丢弃 pending compact 状态（UUID 校验失败时调用）。"""
        if self._pending_compact_task and not self._pending_compact_task.done():
            self._pending_compact_task.cancel()
        self._pending_compact_task = None
        self._pending_compact_result = None
        self._delete_pending_compact()

    async def do_manual_compact(
        self,
        model: str,
        model_provider: ModelProvider,
        current_tools: Callable[[], list[Tool]],
        instruction: str | None = None,
        options: "RequestOptions | None" = None,
    ) -> tuple[int, int]:
        """手动压缩上下文。

        丢弃 pending async compact，对当前消息链执行同步压缩，
        将压缩结果写入消息链。Compact 打破 prefix cache，完成后
        自动同步声明集为 current_tools()（await 后求值，避免并发切换导致过期快照）。

        Args:
            model: 主 model 名称
            model_provider: ModelProvider 实例
            current_tools: 零参 callable，返回 Agent 当前可执行工具
            instruction: 用户下发的压缩侧重指令（/compact <侧重>），透传给 Compactor

        Returns:
            (original_tokens, compressed_tokens)

        Raises:
            RuntimeError: 未配置 compactor 或压缩失败
        """
        if not self.compactor:
            raise RuntimeError("compactor not configured")

        self._discard_pending_compact()
        msgs = self.get_context_window()
        full_context = [self.system_prompt] + msgs

        compacted = await self.compactor.do_compact(
            full_context,
            model,
            model_provider,
            tools=list(self._declared_tools),
            instruction=instruction,
            options=options,
        )

        last_compressed_uuid = msgs[-1].uuid if msgs else None
        compact_node = Message(
            role="assistant",
            content=compacted.content,
            parent_uuid=None,
            unzip_last_uuid=last_compressed_uuid,
        )
        compact_node.uuid = uuid.uuid4().hex

        self._messages.append_detached(compact_node)
        self._messages.set_tip(compact_node.uuid)

        # Compact 打破 prefix cache——同步声明集（await 后求值）
        self._sync_declared_tools(current_tools())

        return compacted.usage.prompt_tokens, compacted.usage.completion_tokens

    # ── Pending compact 持久化 ────────────────────
    # 经由 MessageLog 的 aux kv 通道（key="pending_compact"），
    # 与消息日志同生命周期，CM 不感知任何存储介质细节。

    _PENDING_COMPACT_AUX_KEY = "pending_compact"

    def _persist_pending_compact(self, result: PendingCompact) -> None:
        """持久化 pending compact（经 MessageLog aux 通道）。"""
        data = {
            "compact_content": result.compact_content,
            "start_uuid": result.start_uuid,
            "end_uuid": result.end_uuid,
            "usage": result.usage.model_dump() if result.usage else None,
            "timestamp": result.timestamp,
        }
        try:
            self._messages.write_aux(self._PENDING_COMPACT_AUX_KEY, data)
        except Exception as e:
            log.warning(f"Failed to persist pending compact: {e}")

    def _load_pending_compact(self) -> PendingCompact | None:
        """恢复 pending compact（经 MessageLog aux 通道）。

        损坏数据由 MessageLog 后端丢弃（返回 None）。
        """
        data = self._messages.read_aux(self._PENDING_COMPACT_AUX_KEY)
        if data is None:
            return None
        try:
            usage = None
            if data.get("usage"):
                usage = LLMUsage(**data["usage"])
            return PendingCompact(
                compact_content=data["compact_content"],
                start_uuid=data["start_uuid"],
                end_uuid=data["end_uuid"],
                usage=usage,
                timestamp=data.get("timestamp", ""),
            )
        except Exception as e:
            log.warning(f"Failed to load pending compact, deleting: {e}")
            self._delete_pending_compact()
            return None

    def _delete_pending_compact(self) -> None:
        """删除 pending compact aux 数据。"""
        self._messages.delete_aux(self._PENDING_COMPACT_AUX_KEY)

    def add_message(self, message: Message) -> None:
        self._messages.append(message)

    def add_messages(self, messages: list[Message]) -> None:
        self._messages.extend(iter(messages))

    def append_event(self, event: "WingEvent") -> None:
        """将事件节点追加进混合链（即时落盘，唯一持久化入口）。

        事件与 Message 同链：uuid/parent_uuid 由 TrackedList 填充，
        记录级判别靠 role="event"。调用方（AgentEventSink / runtime）负责
        仅对 persist=true 的事件调用本方法——persist=false 的瞬态事件只
        广播、不落盘、不缓冲。
        """
        self._messages.append(event)

    def _last_prompt_tokens(self) -> int | None:
        """从消息列表中查找最后一条有服务端 usage 的 assistant 消息的 prompt_tokens。

        这是服务端回传的真实 context 大小——优先使用。事件节点不计。
        """
        for node in reversed(self._messages.active_chain):
            if (
                isinstance(node, Message)
                and node.role == "assistant"
                and node.usage is not None
                and node.usage.prompt_tokens > 0
            ):
                return node.usage.prompt_tokens
        return None

    def get_context_window(self) -> list[Message]:
        """返回当前上下文窗口——活跃链的 Message 投影（事件节点过滤）。

        这是 LLM 上下文的唯一视图：get_messages_for_llm、token 统计、
        compact、序列化等所有消费方都经此过滤，事件节点绝不进入模型请求。
        """
        return [m for m in self._messages.active_chain if isinstance(m, Message)]

    def get_active_events(
        self, pending_ask_ids: set[str] | None = None
    ) -> list["WingEvent"]:
        """返回活跃链上**可下发**的事实事件节点（按链序）——前端重放视图。

        过滤策略归属后端单点（前端只做能力分发，不编码"某类不得渲染"）：
        - 仅事实类事件（`FACT_EVENTS`，与 persist 标记同处一文件）下发；
          存量日志里已写入的孪生记录（tool_call_result / llm_call_metrics）
          能加载进链但不在集合中，自然不下发（零迁移，前向容忍）；
        - `AskEvent` 额外按 `pending_ask_ids` 过滤——只下发**仍挂起**的提问
          （已答/已失效的 ask 重放会渲染出活的 Ask 卡，用户回答进虚空）。
          pending_ask_ids 为 None 时不下发任何 ask（调用方未提供待答集合）。

        被压缩/回退区间的事件不在活跃链上，自然不发射（无需孤儿处理）。
        """
        from wing.event import FACT_EVENTS, AskEvent, WingEvent

        result: list[WingEvent] = []
        for e in self._messages.active_chain:
            if not isinstance(e, WingEvent):
                continue
            if e.type not in FACT_EVENTS:
                continue
            if isinstance(e, AskEvent):
                if pending_ask_ids is None or e.tool_call_id not in pending_ask_ids:
                    continue
            result.append(e)
        return result

    def get_context_stats(self) -> tuple[int, int]:
        """获取上下文统计信息：消息数量和 token 数（事件节点不计）。

        优先使用服务端 usage（从 _last_prompt_tokens），
        fallback 到 TokenCounter 估算。
        """
        msgs = self.get_context_window()
        count = len(msgs)
        server_tokens = self._last_prompt_tokens()
        if server_tokens is not None:
            tokens = server_tokens
        else:
            tokens = sum(m.estimate_tokens() for m in msgs)
        return count, tokens

    def rewind(self, target_uuid: str) -> str | None:
        """回退到 target_uuid 之前的状态。

        算法：
        1. 用 find() 在 JSONL 中定位 target 消息
        2. 用 find() 定位 target 的 parent
        3. 构造回退行（复制 parent 的内容，新 uuid，parent_uuid = 祖父 uuid）
        4. append_detached + set_tip
        5. 返回 target.content 作为 draft

        target = "current" → 不操作，返回 None。
        """
        if target_uuid == "current":
            return None

        target = self._messages.find(target_uuid)
        if target is None:
            raise ValueError(f"uuid {target_uuid} not found")
        if not isinstance(target, Message):
            raise ValueError(f"uuid {target_uuid} is not a message node")
        target_msg = target

        draft = target_msg.content or ""

        # 找到 target 的 parent（沿链回溯跳过事件节点——事件是显示注解，
        # 不是上下文状态，rewind 的"回到 parent"语义只对 Message 有定义）
        parent_uuid = target_msg.parent_uuid
        while parent_uuid is not None:
            parent_node = self._messages.find(parent_uuid)
            if parent_node is None:
                raise ValueError(f"parent uuid {parent_uuid} not found")
            if isinstance(parent_node, Message):
                break
            parent_uuid = parent_node.parent_uuid
        if parent_uuid is None:
            # target 是根消息（或其上只有事件节点）：回退到空
            rewind_msg = Message(
                role="system",
                content="[rewind_to_root]",
                parent_uuid=None,
            )
            rewind_msg.uuid = str(uuid.uuid4())
        else:
            parent_msg = self._messages.find(parent_uuid)
            assert isinstance(parent_msg, Message)

            # 构造回退行——`unzip_last_uuid` 必须跟着走：parent 是压缩节点时，
            # 它是"被压缩区间在哪"的唯一编码，丢了会让压缩前区间（乃至整段
            # 历史）从 /rewind、/fork 候选里消失（回退到压缩后第一条消息即触发）。
            rewind_msg = Message(
                role=parent_msg.role,
                content=parent_msg.content,
                reasoning_content=parent_msg.reasoning_content,
                content_blocks=parent_msg.content_blocks,
                tool_calls=parent_msg.tool_calls,
                tool_call_id=parent_msg.tool_call_id,
                parent_uuid=parent_msg.parent_uuid,
                unzip_last_uuid=parent_msg.unzip_last_uuid,
            )
            rewind_msg.uuid = str(uuid.uuid4())

        self._messages.append_detached(rewind_msg)
        self._messages.set_tip(rewind_msg.uuid)
        return draft

    def get_branch_targets(self) -> list[dict]:
        """返回完整链上的 user 消息 + 压缩节点标记 + (current)。

        遍历完整链（walk_full_chain，包含压缩节点与事件节点——事件节点
        无 user role、无 unzip 标记，天然被过滤），取出 user 消息和压缩
        节点（unzip_last_uuid 不为空）。末尾追加 (current) 选项，代表当前
        最新状态。每项格式：{"uuid": str, "content": str}
        """
        result = []
        for node in self._messages.walk_full_chain():
            if not isinstance(node, Message):
                continue
            if node.role == "user" and node.content:
                result.append(
                    {
                        "uuid": node.uuid,
                        "content": (node.content or "")[:100],
                    }
                )
            elif node.unzip_last_uuid is not None:
                result.append(
                    {
                        "uuid": node.uuid,
                        "content": f"[Compact] {(node.content or '')[:80]}",
                    }
                )
        result.append({"uuid": "current", "content": "(current)"})
        return result
