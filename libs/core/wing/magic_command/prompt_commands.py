"""Prompt 类型命令加载器和文本展开。"""

from __future__ import annotations

import glob
from pathlib import Path

import frontmatter

from wing.common.logger import log

from .registry import MagicCommand, magic_registry


def expand_prompt_command(name: str, args: str) -> str | None:
    """将 prompt 命令名和参数展开为完整的 prompt 文本。

    查找 registry 中的命令元数据，读取 .md 文件正文（不含 frontmatter），
    替换 $ARGUMENTS 后返回展开文本。

    Args:
        name: 命令名（不含 / 前缀），如 "plan"
        args: 用户提供的参数

    Returns:
        展开后的 prompt 文本，或 None（命令名不匹配任何 prompt 命令）
    """
    cmd = magic_registry.get(name)
    if cmd is None or cmd.source != "prompt" or cmd.file_path is None:
        return None

    md_path = Path(cmd.file_path)
    try:
        with md_path.open("r", encoding="utf-8") as f:
            post = frontmatter.load(f)
        content = post.content
    except Exception as e:
        log.error(f"Failed to read prompt command file {md_path}: {e}")
        return None

    # 替换 $ARGUMENTS 变量
    final_content = content.replace("$ARGUMENTS", args)

    # 确保不以 "/" 开头避免被误识别为命令
    if final_content.strip().startswith("/"):
        final_content = " " + final_content

    if args:
        return f"{final_content} <user_input>{args}</user_input>"
    return final_content


def load_prompt_command_from_file(md_path: Path) -> MagicCommand | None:
    """从 md 文件加载 prompt 类型命令元数据。

    只加载 frontmatter 中的元数据（name/description/aliases），
    不创建 handler。展开由 expand_prompt_command() 负责。
    """
    try:
        with md_path.open("r", encoding="utf-8") as f:
            post = frontmatter.load(f)

        original_name = post.get("name")
        if not original_name:
            log.warning(f"Prompt command {md_path} missing required 'name' field")
            return None

        original_name_str = str(original_name)
        normalized_name = original_name_str.replace(" ", "")

        description = str(post.get("description", ""))
        aliases_raw = post.get("aliases", [])

        if not isinstance(aliases_raw, list):
            log.warning(
                f"Prompt command {md_path} 'aliases' should be a list, got {type(aliases_raw)}"
            )
            aliases = []
        else:
            aliases = [str(a) for a in aliases_raw]

        return MagicCommand(
            name=normalized_name,
            description=description,
            aliases=aliases,
            params="[args]",
            source="prompt",
            file_path=str(md_path),
        )

    except Exception as e:
        log.error(f"Failed to load prompt command from {md_path}: {e}")
        return None


def load_prompt_commands_from_paths(paths: list[str]) -> list[MagicCommand]:
    """从多个 glob 路径加载 prompt 类型命令。"""
    commands: list[MagicCommand] = []

    for path_pattern in paths:
        expanded_pattern = str(Path(path_pattern).expanduser())
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
    """加载并注册 prompt 类型命令到全局 registry。"""
    commands = load_prompt_commands_from_paths(paths)

    for cmd in commands:
        magic_registry.register_command(cmd)
        alias_str = f" (aliases: {', '.join(cmd.aliases)})" if cmd.aliases else ""
        log.debug(f"Registered prompt command: /{cmd.name}{alias_str}")
