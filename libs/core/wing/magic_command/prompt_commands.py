"""Prompt 类型命令加载器."""

from __future__ import annotations

import glob
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING, Any

import frontmatter

from wing.common.logger import log

from .registry import MagicCommand, magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


def _create_prompt_handler(md_path: Path) -> Callable[..., Any]:
    """创建 prompt 类型命令的 handler。

    Args:
        md_path: markdown 文件路径

    Returns:
        异步处理函数
    """

    async def handler(agent: "WingAgent", args: str) -> str:
        """读取 md 文件内容，和 args 拼接后作为 user 消息发送。"""
        try:
            # 读取完整文件内容（包括 frontmatter）
            content = md_path.read_text(encoding="utf-8")

            # 替换 $ARGUMENTS 变量
            final_content = content.replace("$ARGUMENTS", args)

            # TODO：这里要优化一下，解耦agent和 magic command系统
            # 构建最终 prompt，确保不以 "/" 开头避免被误认为 magic command
            # 添加一个空格在最前面（如果内容以"/"开头）
            if final_content.strip().startswith("/"):
                final_content = " " + final_content

            if args:
                final_prompt = f"{final_content} <user_input>{args}</user_input>"
            else:
                final_prompt = final_content

            # 以 user 身份发送给 agent
            await agent.post(content=final_prompt, role="user")

            # 返回提示信息
            return f"✅ 已加载 prompt 命令: {md_path.name}"

        except Exception as e:
            log.error(f"Failed to execute prompt command {md_path}: {e}")
            return f"❌ 执行命令失败: {e}"

    return handler


def load_prompt_command_from_file(md_path: Path) -> MagicCommand | None:
    """从 md 文件加载 prompt 类型命令。

    md 文件格式：
    ---
    name: command-name (必填)
    description: command description (选填)
    aliases: [alias1, alias2] (选填)
    ---

    命令正文内容...

    Args:
        md_path: markdown 文件路径

    Returns:
        MagicCommand 实例，如果加载失败返回 None
    """
    try:
        with md_path.open("r", encoding="utf-8") as f:
            post = frontmatter.load(f)

        # name 是必填字段
        original_name = post.get("name")
        if not original_name:
            log.warning(f"Prompt command {md_path} missing required 'name' field")
            return None

        # 规范化命令名：去除空格并转为小写
        # 例如："OPSX: Apply" -> "opsx:apply"
        original_name_str = str(original_name)
        normalized_name = original_name_str.replace(" ", "")

        # description 和 aliases 是选填字段
        description = str(post.get("description", ""))
        aliases_raw = post.get("aliases", [])

        # 验证 aliases 是列表
        if not isinstance(aliases_raw, list):
            log.warning(
                f"Prompt command {md_path} 'aliases' should be a list, got {type(aliases_raw)}"
            )
            aliases = []
        else:
            # 确保所有alias都是字符串
            aliases = [str(a) for a in aliases_raw]

        # 创建 handler
        handler = _create_prompt_handler(md_path)

        # 创建 MagicCommand
        return MagicCommand(
            name=normalized_name,
            description=description,
            aliases=aliases,
            params="[args]",  # prompt 命令接受可选参数
            handler=handler,
            source="prompt",
        )

    except Exception as e:
        log.error(f"Failed to load prompt command from {md_path}: {e}")
        return None


def load_prompt_commands_from_paths(paths: list[str]) -> list[MagicCommand]:
    """从多个 glob 路径加载 prompt 类型命令。

    Args:
        paths: glob 路径列表（支持 ~ 展开和 ** 模式）

    Returns:
        加载成功的 MagicCommand 列表
    """
    commands: list[MagicCommand] = []

    for path_pattern in paths:
        # 展开 ~ 和环境变量
        expanded_pattern = str(Path(path_pattern).expanduser())

        # 使用 glob.glob 支持绝对路径
        matched_files = glob.glob(expanded_pattern, recursive=True)

        for matched_path in matched_files:
            md_path = Path(matched_path)
            if not md_path.is_file():
                continue

            cmd = load_prompt_command_from_file(md_path)
            if cmd:
                commands.append(cmd)
                log.info(f"Loaded prompt command: /{cmd.name} from {md_path}")

    return commands


def register_prompt_commands(paths: list[str]) -> None:
    """加载并注册 prompt 类型命令到全局 registry。

    Args:
        paths: glob 路径列表
    """
    commands = load_prompt_commands_from_paths(paths)

    for cmd in commands:
        magic_registry.register_command(cmd)
        alias_str = f" (aliases: {', '.join(cmd.aliases)})" if cmd.aliases else ""
        log.debug(f"Registered prompt command: /{cmd.name}{alias_str}")
