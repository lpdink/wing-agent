"""测试 prompt 类型命令加载器和文本展开."""

from pathlib import Path

from wing.magic_command.prompt_commands import (
    expand_prompt_command,
    load_prompt_command_from_file,
    load_prompt_commands_from_paths,
    register_prompt_commands,
)
from wing.magic_command.registry import magic_registry


class TestLoadPromptCommandFromFile:
    """测试从单个文件加载命令."""

    def test_load_valid_command(self, tmp_path: Path):
        """测试加载有效的命令文件."""
        cmd_file = tmp_path / "test.md"
        cmd_file.write_text(
            """---
name: test-command
description: A test command
aliases:
  - tc
  - t
---

This is the prompt content.
$ARGUMENTS
""",
            encoding="utf-8",
        )

        cmd = load_prompt_command_from_file(cmd_file)

        assert cmd is not None
        # 名称被规范化：去空格、转小写
        assert cmd.name == "test-command"
        assert cmd.description == "A test command"
        assert "tc" in cmd.aliases
        assert "t" in cmd.aliases
        assert cmd.params == "[args]"

    def test_load_command_with_space_in_name(self, tmp_path: Path):
        """测试加载名称包含空格的命令."""
        cmd_file = tmp_path / "space.md"
        cmd_file.write_text(
            """---
name: "OPSX: Apply"
description: A command with space
---

Content
""",
            encoding="utf-8",
        )

        cmd = load_prompt_command_from_file(cmd_file)

        assert cmd is not None
        # 名称被规范化：去空格
        assert cmd.name == "OPSX:Apply"

    def test_load_command_without_name(self, tmp_path: Path):
        """测试加载缺少 name 字段的文件."""
        cmd_file = tmp_path / "no_name.md"
        cmd_file.write_text(
            """---
description: A test command
---

This is the prompt content.
""",
            encoding="utf-8",
        )

        cmd = load_prompt_command_from_file(cmd_file)

        assert cmd is None

    def test_load_command_without_frontmatter(self, tmp_path: Path):
        """测试加载没有 frontmatter 的文件."""
        cmd_file = tmp_path / "no_fm.md"
        cmd_file.write_text(
            "Just some content without frontmatter",
            encoding="utf-8",
        )

        cmd = load_prompt_command_from_file(cmd_file)

        # 应该返回 None，因为没有 name 字段
        assert cmd is None

    def test_load_command_with_invalid_aliases(self, tmp_path: Path):
        """测试 aliases 不是列表的情况."""
        cmd_file = tmp_path / "invalid_aliases.md"
        cmd_file.write_text(
            """---
name: test
aliases: "not-a-list"
---

Content
""",
            encoding="utf-8",
        )

        cmd = load_prompt_command_from_file(cmd_file)

        # 应该能加载，但 aliases 为空列表
        assert cmd is not None
        assert cmd.name == "test"
        assert cmd.aliases == []

    def test_load_command_with_no_description(self, tmp_path: Path):
        """测试没有 description 的情况."""
        cmd_file = tmp_path / "no_desc.md"
        cmd_file.write_text(
            """---
name: test
---

Content
""",
            encoding="utf-8",
        )

        cmd = load_prompt_command_from_file(cmd_file)

        assert cmd is not None
        assert cmd.name == "test"
        assert cmd.description == ""

    def test_handler_execution(self, tmp_path: Path):
        """测试 handler 的执行."""
        cmd_file = tmp_path / "test.md"
        cmd_file.write_text(
            """---
name: test
---

Content with $ARGUMENTS
""",
            encoding="utf-8",
        )

        cmd = load_prompt_command_from_file(cmd_file)

        assert cmd is not None
        assert cmd.file_path is not None
        assert cmd.source == "prompt"


class TestLoadPromptCommandsFromPaths:
    """测试从多个路径加载命令."""

    def test_load_from_multiple_paths(self, tmp_path: Path):
        """测试从多个路径加载命令."""
        # 创建第一个目录和文件
        dir1 = tmp_path / "dir1"
        dir1.mkdir()
        cmd1 = dir1 / "cmd1.md"
        cmd1.write_text(
            """---
name: cmd1
description: Command 1
---

Content 1
""",
            encoding="utf-8",
        )

        # 创建第二个目录和文件
        dir2 = tmp_path / "dir2"
        dir2.mkdir()
        cmd2 = dir2 / "cmd2.md"
        cmd2.write_text(
            """---
name: cmd2
description: Command 2
---

Content 2
""",
            encoding="utf-8",
        )

        # 加载
        paths = [str(dir1 / "*.md"), str(dir2 / "*.md")]
        commands = load_prompt_commands_from_paths(paths)

        assert len(commands) == 2
        names = [c.name for c in commands]
        assert "cmd1" in names
        assert "cmd2" in names

    def test_load_with_glob_pattern(self, tmp_path: Path):
        """测试使用 glob 模式加载."""
        # 创建嵌套目录结构
        subdir = tmp_path / "commands" / "subdir"
        subdir.mkdir(parents=True)

        cmd1 = tmp_path / "commands" / "cmd1.md"
        cmd1.write_text(
            """---
name: cmd1
---

Content 1
""",
            encoding="utf-8",
        )

        cmd2 = subdir / "cmd2.md"
        cmd2.write_text(
            """---
name: cmd2
---

Content 2
""",
            encoding="utf-8",
        )

        # 使用 ** 模式
        paths = [str(tmp_path / "commands" / "**" / "*.md")]
        commands = load_prompt_commands_from_paths(paths)

        assert len(commands) == 2
        names = [c.name for c in commands]
        assert "cmd1" in names
        assert "cmd2" in names

    def test_load_with_expanduser(self, tmp_path: Path, monkeypatch):
        """测试路径展开 (~ 展开)."""
        # 设置 HOME 环境变量
        monkeypatch.setenv("HOME", str(tmp_path))

        # 创建 ~/.wing/commands 目录
        cmd_dir = tmp_path / ".wing" / "commands"
        cmd_dir.mkdir(parents=True)

        cmd_file = cmd_dir / "test.md"
        cmd_file.write_text(
            """---
name: test
---

Content
""",
            encoding="utf-8",
        )

        # 使用 ~ 路径
        paths = ["~/.wing/commands/*.md"]
        commands = load_prompt_commands_from_paths(paths)

        assert len(commands) == 1
        assert commands[0].name == "test"


class TestRegisterPromptCommands:
    """测试命令注册."""

    def test_register_commands(self, tmp_path: Path):
        """测试注册命令到 registry."""
        # 创建测试命令
        cmd_file = tmp_path / "register_test.md"
        cmd_file.write_text(
            """---
name: register-test
description: Test registration
aliases:
  - rt
---

Content
""",
            encoding="utf-8",
        )

        # 注册

        register_prompt_commands([str(tmp_path / "*.md")])

        # 验证注册成功
        cmd = magic_registry.get("register-test")
        assert cmd is not None
        assert cmd.name == "register-test"

        # 验证别名也注册成功
        cmd_alias = magic_registry.get("rt")
        assert cmd_alias is not None
        assert cmd_alias.name == "register-test"

        # 清理测试命令
        if "register-test" in magic_registry._commands:
            del magic_registry._commands["register-test"]
        if "rt" in magic_registry._commands:
            del magic_registry._commands["rt"]


class TestExpandPromptCommand:
    """测试 prompt 命令文本展开。"""

    def test_expand_with_args(self, tmp_path: Path):
        """匹配成功且有参数时正确展开。"""
        md_file = tmp_path / "plan.md"
        md_file.write_text(
            "---\nname: plan\ndescription: Create a plan\n---\n\nPlan for $ARGUMENTS",
            encoding="utf-8",
        )
        cmd = load_prompt_command_from_file(md_file)
        assert cmd is not None
        magic_registry.register_command(cmd)
        try:
            result = expand_prompt_command("plan", "auth feature")
            assert result is not None
            assert "Plan for auth feature" in result
            assert "<user_input>auth feature</user_input>" in result
            # frontmatter 不应出现在展开文本中
            assert "name: plan" not in result
        finally:
            magic_registry.remove_by_source("prompt")

    def test_expand_no_args(self, tmp_path: Path):
        """无参数时 $ARGUMENTS 替换为空。"""
        md_file = tmp_path / "review.md"
        md_file.write_text(
            "---\nname: review\n---\n\nReview $ARGUMENTS code",
            encoding="utf-8",
        )
        cmd = load_prompt_command_from_file(md_file)
        assert cmd is not None
        magic_registry.register_command(cmd)
        try:
            result = expand_prompt_command("review", "")
            assert result is not None
            assert "Review  code" in result
            assert "<user_input>" not in result
        finally:
            magic_registry.remove_by_source("prompt")

    def test_expand_unknown_command(self):
        """不匹配任何命令时返回 None。"""
        result = expand_prompt_command("nonexistent-cmd-xyz", "args")
        assert result is None

    def test_expand_slash_prefix_protection(self, tmp_path: Path):
        """展开文本以 / 开头时前置空格防止误识别。"""
        md_file = tmp_path / "slash.md"
        md_file.write_text(
            "---\nname: slashtest\n---\n\n/this starts with slash",
            encoding="utf-8",
        )
        cmd = load_prompt_command_from_file(md_file)
        assert cmd is not None
        magic_registry.register_command(cmd)
        try:
            result = expand_prompt_command("slashtest", "")
            assert result is not None
            assert result.startswith(" /")
        finally:
            magic_registry.remove_by_source("prompt")

    def test_expand_missing_file(self, tmp_path: Path):
        """文件不存在时返回 None。"""
        md_file = tmp_path / "gone.md"
        md_file.write_text("---\nname: gone\n---\n\ncontent", encoding="utf-8")
        cmd = load_prompt_command_from_file(md_file)
        assert cmd is not None
        magic_registry.register_command(cmd)
        # 删除文件
        md_file.unlink()
        try:
            result = expand_prompt_command("gone", "")
            assert result is None
        finally:
            magic_registry.remove_by_source("prompt")
