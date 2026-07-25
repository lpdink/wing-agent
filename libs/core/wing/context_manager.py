# wing/context_manager.py
import asyncio
import glob
import os
import uuid
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

import frontmatter

from .common.logger import log
from .common.tracked_list import TrackedList
from .compactor import Compactor
from .openai_provider import OpenAIProvider
from .schema import AgentSkill, LLMUsage, Message


@dataclass
class PendingCompact:
    """预计算的压缩结果，等待 apply 到消息链。

    start_uuid / end_uuid 用于 UUID 自校验：apply 时在当前活跃链中查找，
    找不到则说明链已被修改（rewind / 手动 compact），自动丢弃。
    """

    compact_content: str
    start_uuid: str
    end_uuid: str
    usage: LLMUsage | None = None
    timestamp: str = field(default_factory=lambda: datetime.now().isoformat())


class ContextManager:
    _DEFAULT_AGENT_SKILL_INSTRUCTION = (
        "# Agent Skills\n"
        "The agent skills are a collection of folds of instructions, scripts, "
        "and resources that you can load dynamically to improve performance "
        "on specialized tasks. Each agent skill has a `SKILL.md` file in its "
        "folder that describes how to use the skill. "
        "Read its `SKILL.md` file if necessarily."
    )
    _DEFAULT_AGENT_SKILL_TEMPLATE = """## {name}
{description}
More detail in: "{dir}/SKILL.md" """

    def __init__(
        self,
        session_id: str,
        messages: TrackedList[Message],
        system_prompt: str,
        compactor: Compactor,
        skills_patterns: list[str] | None = None,
        rules_patterns: list[str] | None = None,
        workspace: str | None = None,
    ) -> None:
        """
        Args:
            session_id: Session 标识符（只读，不用于生成或路径计算）
            messages: TrackedList 消息列表（外部创建，不感知外存路径）
            system_prompt: 系统提示词
            compactor: 压缩助手（必填）
            skills_patterns: Skills glob 模式列表
            rules_patterns: Rules glob 模式列表
            workspace: 工作目录，相对路径的 patterns 基于此解析
        """
        self.compactor = compactor
        self.setin_system_prompt = system_prompt

        # 接收外部传入的 session_id 和 messages
        self._session_id = session_id
        self._messages = messages
        self._workspace = Path(workspace).resolve() if workspace else None

        # 使用传入的 patterns（不再从全局 config 读取）
        self._skills_patterns = skills_patterns or []
        self._rules_patterns = rules_patterns or []

        # 加载 rules 和 skills（在初始化时加载一次）
        self._rules_prompt = self._load_rules()
        self._skills_cache: dict[str, AgentSkill] = self._load_all_skills()
        self._skills_prompt = self._build_skills_prompt()
        # 由外部（hook）注入的系统提示词片段
        self.inject_system_prompts: list[str] = []

        # ── 异步 compact 状态 ──────────────────────
        self._pending_compact_task: asyncio.Task[None] | None = None
        self._pending_compact_result: PendingCompact | None = None
        # 尝试从磁盘恢复 pending compact（进程重启场景）
        self._pending_compact_result = self._load_pending_compact()

    @property
    def id(self) -> str:
        """当前 session 的标识符。"""
        return self._session_id

    @property
    def system_prompt(self) -> Message:
        """构造完整系统提示词，按顺序拼接：inject + system_prompt + rules + skills"""
        parts = list(self.inject_system_prompts)
        if self.setin_system_prompt:
            parts.append(self.setin_system_prompt)
        if self._rules_prompt:
            parts.append(self._rules_prompt)
        if self._skills_prompt:
            parts.append(self._skills_prompt)
        return Message(role="system", content="\n\n".join(parts))

    def reload_skills_and_rules(self) -> None:
        """重新加载 skills 和 rules，用于 /reload 命令。"""
        self._rules_prompt = self._load_rules()
        self._skills_cache = self._load_all_skills()
        self._skills_prompt = self._build_skills_prompt()

    def _resolve_patterns(self, patterns: list[str]) -> list[str]:
        """将 glob patterns 解析为实际路径列表。

        - ~ 路径: expanduser
        - 绝对路径: 保持不变
        - 相对路径: 基于 workspace 解析（无 workspace 则保持原样，由 glob 对 cwd 展开）
        """
        resolved: list[str] = []
        for p in patterns:
            expanded = os.path.expanduser(p)
            if self._workspace and not os.path.isabs(expanded):
                resolved.append(str(self._workspace / expanded))
            else:
                resolved.append(expanded)
        return resolved

    def _load_rules(self) -> str:
        """加载所有 rules 文件并拼接。

        rules 配置支持 glob 模式，如 ~/.wing/rules/*.md
        相对路径基于 workspace 解析。
        文件不存在或读取失败时 log.warning 并跳过。
        """
        all_patterns = self._resolve_patterns(self._rules_patterns)
        if not all_patterns:
            return ""

        contents = []
        for pattern in all_patterns:
            matched_files = sorted(glob.glob(pattern))

            for file_path in matched_files:
                try:
                    content = Path(file_path).read_text(encoding="utf-8")
                    contents.extend([file_path, content])
                except Exception as e:
                    log.warning(f"Failed to read rules file {file_path}: {e}")

        return "\n\n".join(contents)

    def _load_all_skills(self) -> dict[str, AgentSkill]:
        """从所有 skills glob patterns 加载技能。

        支持 * 单层目录匹配和 ** 多层目录匹配。
        相对路径基于 workspace 解析。
        每个匹配到的 .md 文件必须包含 name 和 description Front Matter。
        同名 skill 冲突处理：按 glob 结果排序后加载第一个，其他 log.warning 跳过。
        """
        all_skills: dict[str, AgentSkill] = {}
        seen_names: dict[str, str] = {}  # 记录已加载的 skill 名及其来源路径

        all_patterns = self._resolve_patterns(self._skills_patterns)

        for pattern in all_patterns:
            matched_files = sorted(glob.glob(pattern, recursive=True))

            for skill_md_path in matched_files:
                skill_md = Path(skill_md_path)
                if not skill_md.is_file():
                    continue

                skill_dir = skill_md.parent
                skill = self._parse_skill(skill_dir, skill_md)
                if skill is None:
                    continue

                if skill.name in seen_names:
                    log.warning(
                        f"Skill '{skill.name}' already loaded from {seen_names[skill.name]}, "
                        f"skipping duplicate in {skill_dir}"
                    )
                    continue

                seen_names[skill.name] = str(skill_dir)
                all_skills[skill.name] = skill

        return all_skills

    def _parse_skill(
        self, skill_dir: Path, skill_md: Path | None = None
    ) -> AgentSkill | None:
        """解析单个 skill 目录的 SKILL.md 文件。"""
        if skill_md is None:
            skill_md = skill_dir / "SKILL.md"
        if not skill_md.is_file():
            log.warning(
                f"The skill directory '{skill_dir}' must include a SKILL.md file."
            )
            return None

        try:
            with skill_md.open("r", encoding="utf-8") as f:
                post = frontmatter.load(f)

            name = post.get("name")
            description = post.get("description")

            if not name or not description:
                log.warning(
                    f"The SKILL.md in '{skill_dir}' must have YAML Front Matter "
                    "with 'name' and 'description' fields."
                )
                return None

            return AgentSkill(
                name=str(name),
                description=str(description),
                dir=str(skill_dir),
            )
        except Exception as e:
            log.warning(f"Failed to parse SKILL.md in '{skill_dir}': {e}")
            return None

    def _build_skills_prompt(self) -> str:
        """构建 skills 提示词部分。"""
        if not self._skills_cache:
            return ""

        skill_descriptions = [
            ContextManager._DEFAULT_AGENT_SKILL_INSTRUCTION,
        ] + [
            ContextManager._DEFAULT_AGENT_SKILL_TEMPLATE.format(
                name=skill.name,
                description=skill.description,
                dir=skill.dir,
            )
            for skill in self._skills_cache.values()
        ]
        return "\n".join(skill_descriptions)

    def get_skills_info(self) -> str:
        """返回 skills 信息，用于 /skills 命令显示。"""
        lines = []

        # 显示 skills patterns
        if self._skills_patterns:
            lines.append("📚 Skills patterns:")
            for pattern in self._skills_patterns:
                lines.append(f"  - {pattern}")
            lines.append("")

        # 显示已加载的 skills
        if self._skills_cache:
            lines.append("已加载的 Skills:")
            for skill in self._skills_cache.values():
                lines.append(f"  {skill.name}: {skill.description}")
        else:
            lines.append("暂无已加载的 Skills")

        return "\n".join(lines)

    async def get_messages_for_llm(
        self,
        model: str,
        model_provider: OpenAIProvider,
        tools: list | None = None,
    ) -> list[Message]:
        """Get messages ready for LLM API call.

        异步 compact 三步流程（无同步 fallback）：
        1. 有 pending result + tokens >= context_window_tokens → 尝试 apply
        2. 有 running task → 等待（这次不 apply）
        3. tokens >= compact_window_tokens → 启动后台 compact task

        Args:
            model: 主 model 名称（触发 compact 时透传给 provider）。
            model_provider: OpenAIProvider 实例（触发 compact 时使用）。
        """
        if not self.compactor:
            return [self.system_prompt] + self._messages.active_chain

        msgs = self._messages.active_chain
        server_tokens = self._last_prompt_tokens()

        # ── Step 1: 有预计算结果？尝试 apply ──
        if self._pending_compact_result is not None:
            if self.compactor.need_apply_compact(msgs, server_tokens):
                # 等后台 task 跑完（如果还在跑）
                if self._pending_compact_task and not self._pending_compact_task.done():
                    try:
                        await self._pending_compact_task
                    except Exception:
                        pass

                if self._pending_compact_result is not None:
                    indices = self._verify_snapshot_valid(msgs)
                    if indices is not None:
                        self._apply_pending_compact(msgs, *indices)
                        return [self.system_prompt] + self._messages.active_chain
                    else:
                        # UUID 不匹配（rewind 等），丢弃
                        self._discard_pending_compact()
            else:
                # 已有预计算结果，但还没到 apply 阈值——直接返回，
                # 不再走 Step 2/3，避免重复启动 compact task
                return [self.system_prompt] + msgs

        # ── Step 2: task 还在跑？ ──
        if self._pending_compact_task is not None:
            if self._pending_compact_task.done():
                # task 完成了但 result 没设上（失败了），清掉
                self._pending_compact_task = None
            else:
                # task 还在跑，返回原始消息
                return [self.system_prompt] + msgs

        # ── Step 3: 该触发 early compact 了？ ──
        if self.compactor.need_early_trigger(msgs, server_tokens):
            self._start_background_compact(msgs, model, model_provider, tools)

        return [self.system_prompt] + msgs

    # ── 异步 compact 内部方法 ─────────────────────

    def _start_background_compact(
        self,
        msgs: list[Message],
        model: str,
        model_provider: OpenAIProvider,
        tools: list | None,
    ) -> None:
        """启动后台异步 compact task。"""
        preserve_last = msgs[-1].role == "user" if msgs else False
        head = msgs[:-1] if preserve_last else msgs
        cut_idx = self.compactor._calc_cut_idx(head)

        if cut_idx == 0:
            return  # 没有消息可压缩

        start_uuid: str = head[0].uuid  # ty: ignore[invalid-assignment]
        end_uuid: str = head[cut_idx - 1].uuid  # ty: ignore[invalid-assignment]
        full_context = [self.system_prompt] + head[:cut_idx]

        async def _run() -> None:
            try:
                response = await self.compactor.do_compact(
                    full_context,
                    model,
                    model_provider,
                    tools=tools,
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

    def _verify_snapshot_valid(self, msgs: list[Message]) -> tuple[int, int] | None:
        """在当前消息列表中查找 pending compact 的 start/end UUID。

        返回 (start_idx, end_idx) 或 None（UUID 找不到）。
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
            return (start_idx, end_idx)
        return None

    def _apply_pending_compact(
        self, msgs: list[Message], start_idx: int, end_idx: int
    ) -> None:
        """将预计算的 compact 结果换入消息链。

        替换 msgs[start_idx : end_idx+1] 为 compact_node，
        relink msgs[end_idx+1 :]。
        """
        pc = self._pending_compact_result
        if pc is None:
            raise RuntimeError("apply called without pending compact result")

        # 构造压缩节点
        compact_node = Message(
            role="assistant",
            content=pc.compact_content,
            parent_uuid=None,
            unzip_last_uuid=pc.end_uuid,
        )
        compact_node.uuid = str(uuid.uuid4())

        # 构造 relink tail（end_idx 之后的消息）
        tail = msgs[end_idx + 1 :]
        relink_tail: list[Message] = []
        prev_uuid: str | None = compact_node.uuid
        for msg in tail:
            relinked = Message(
                role=msg.role,
                content=msg.content,
                reasoning_content=msg.reasoning_content,
                tool_calls=msg.tool_calls,
                tool_call_id=msg.tool_call_id,
                usage=msg.usage,
                parent_uuid=prev_uuid,
            )
            relinked.uuid = str(uuid.uuid4())
            relink_tail.append(relinked)
            prev_uuid = relinked.uuid

        # 写入 JSONL
        # TODO(crash-safety): append_detached(compact_node) 和 set_tip 之间存在
        # 崩溃窗口。如果进程在 compact_node 写入后、relink tail set_tip 前被杀，
        # compact_node（parent_uuid=None）会成为新根，trace_chain 从它回溯到
        # None 就停了，原始 tail 消息从活跃链中丢失。这不是 async compact 引入
        # 的新问题——旧的同步 compact 也有同样的窗口。彻底修复需要 TrackedList
        # 支持批量原子写入（先写所有节点，再原子切换 tip）。
        self._messages.append_detached(compact_node)
        for msg in relink_tail:
            self._messages.append_detached(msg)

        # 切换活跃链末尾
        if relink_tail:
            self._messages.set_tip(relink_tail[-1].uuid)  # ty: ignore[invalid-argument-type]
        else:
            self._messages.set_tip(compact_node.uuid)

        # 清理状态
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
        model_provider: OpenAIProvider,
        tools: list | None = None,
    ) -> tuple[int, int]:
        """手动压缩上下文。

        丢弃 pending async compact，对当前消息链执行同步压缩，
        将压缩结果写入消息链。

        Args:
            model: 主 model 名称
            model_provider: OpenAIProvider 实例
            tools: 可用工具列表

        Returns:
            (original_tokens, compressed_tokens)

        Raises:
            RuntimeError: 未配置 compactor 或压缩失败
        """
        if not self.compactor:
            raise RuntimeError("compactor not configured")

        self._discard_pending_compact()
        msgs = list(self._messages)
        full_context = [self.system_prompt] + msgs

        compacted = await self.compactor.do_compact(
            full_context, model, model_provider, tools=tools
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

    def _last_prompt_tokens(self) -> int | None:
        """从消息列表中查找最后一条有服务端 usage 的 assistant 消息的 prompt_tokens。

        这是服务端回传的真实 context 大小——优先使用。
        """
        for msg in reversed(self._messages):
            if (
                msg.role == "assistant"
                and msg.usage is not None
                and msg.usage.prompt_tokens > 0
            ):
                return msg.usage.prompt_tokens
        return None

    def get_context_window(self) -> list[Message]:
        """返回当前上下文窗口（活跃链）。"""
        return self._messages.active_chain

    def get_context_stats(self) -> tuple[int, int]:
        """获取上下文统计信息：消息数量和 token 数。

        优先使用服务端 usage（从 _last_prompt_tokens），
        fallback 到 TokenCounter 估算。
        """
        count = len(self._messages)
        server_tokens = self._last_prompt_tokens()
        if server_tokens is not None:
            tokens = server_tokens
        else:
            tokens = sum(m.estimate_tokens() for m in self._messages)
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

        # 用 find() 定位 target
        target_msg = self._messages.find(target_uuid)
        if target_msg is None:
            raise ValueError(f"uuid {target_uuid} not found")

        draft = target_msg.content or ""

        # 找到 target 的 parent
        parent_uuid = target_msg.parent_uuid
        if parent_uuid is None:
            # target 是根消息：回退到空
            rewind_msg = Message(
                role="system",
                content="[rewind_to_root]",
                parent_uuid=None,
            )
            rewind_msg.uuid = str(uuid.uuid4())
        else:
            # 用 find() 定位 parent
            parent_msg = self._messages.find(parent_uuid)
            if parent_msg is None:
                raise ValueError(f"parent uuid {parent_uuid} not found")

            # 构造回退行
            rewind_msg = Message(
                role=parent_msg.role,
                content=parent_msg.content,
                reasoning_content=parent_msg.reasoning_content,
                tool_calls=parent_msg.tool_calls,
                tool_call_id=parent_msg.tool_call_id,
                parent_uuid=parent_msg.parent_uuid,  # 祖父 uuid
            )
            rewind_msg.uuid = str(uuid.uuid4())

        self._messages.append_detached(rewind_msg)
        self._messages.set_tip(rewind_msg.uuid)
        return draft

    def extract_subchain(self, target_uuid: str) -> tuple[list[Message], str | None]:
        """提取 target_uuid 之前的完整子链（不含目标消息），用于 fork 到新 session。

        - target = "current" → 复制整个完整链（walk_full_chain），draft 为空字符串
        - 其他 → 用 find() 定位 target，用 walk_full_chain(from_uuid=target.parent_uuid)
          构建完整子链（包含压缩节点，保留拓扑结构）

        返回 (subchain_messages, draft_content)，draft 为目标消息的 content。
        """
        if target_uuid == "current":
            return self._messages.walk_full_chain(), ""

        # 用 find() 定位 target
        target_msg = self._messages.find(target_uuid)
        if target_msg is None:
            raise ValueError(f"uuid {target_uuid} not found")

        draft = target_msg.content or ""

        # 从 target 的 parent 开始构建完整子链
        if target_msg.parent_uuid is None:
            return [], draft

        subchain = self._messages.walk_full_chain(from_uuid=target_msg.parent_uuid)
        return subchain, draft

    def get_branch_targets(self) -> list[dict]:
        """返回完整链上的 user 消息 + 压缩节点标记 + (current)。

        遍历完整链（walk_full_chain，包含压缩节点），
        过滤出 user 消息和压缩节点（unzip_last_uuid 不为空）。
        末尾追加 (current) 选项，代表当前最新状态。
        每个返回项格式：{"uuid": str, "content": str}
        """
        result = []
        for msg in self._messages.walk_full_chain():
            if msg.role == "user" and msg.content:
                result.append(
                    {
                        "uuid": msg.uuid,
                        "content": (msg.content or "")[:100],
                    }
                )
            elif msg.unzip_last_uuid is not None:
                # 压缩节点标记
                result.append(
                    {
                        "uuid": msg.uuid,
                        "content": f"[Compact] {(msg.content or '')[:80]}",
                    }
                )
        result.append({"uuid": "current", "content": "(current)"})
        return result

    def clear_reasoning(self) -> None:
        for msg in self._messages:
            if isinstance(msg, Message):
                msg.reasoning_content = ""
