"""experimental 命名域工具（HashlineRead / HashlineEdit）——工具层契约。

被守的语义：

- 注册身份：namespace ``experimental`` + LLM 可见名 ``HashlineRead`` / ``HashlineEdit``
  ——与主 Read / Edit 并存不撞名（``_bind_tools`` 的 LLM 名唯一性约束）；
- HashlineRead：头部带 TAG、正文 ``N→text``、offset/limit 语义；
- HashlineEdit：tag 对账（过期即拒绝、零写入）、先全量校验后统一落盘、
  结果字符串回执、每个操作一条窗口化 ``diff_content`` 事件（tool_call_id 关联）、
  命名寄存器跨调用（会话级）持久；BOM 读写还原。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.schema import ToolError
from wing.tool_registry import tool_registry
from wing.tools.experimental.edit import hashline_edit
from wing.tools.experimental.hashline import content_tag
from wing.tools.experimental.read import hashline_read


class _StubCtx:
    """Minimal ToolContext stub capturing emitted events."""

    def __init__(self, session_id: str = "sess-hashline") -> None:
        self.session_id = session_id
        self.cwd = None
        self.events: list = []

    def emit(self, event) -> None:
        self.events.append(event)


async def read(ctx: _StubCtx, path: str, **kwargs) -> str:
    return await hashline_read(path, ctx=ctx, **kwargs)  # type: ignore[arg-type]


async def edit(ctx: _StubCtx, payload: str) -> str:
    return await hashline_edit(payload, ctx=ctx)  # type: ignore[arg-type]


def _tool(name: str, namespace: str = "default"):
    tool = tool_registry.get_tool(name, namespace)
    assert tool is not None, f"tool {namespace}.{name} is not registered"
    return tool


# ── 注册身份 ──────────────────────────────────────────────────────────


class TestRegistration:
    def test_namespaced_tools_registered(self):
        edit_tool = _tool("Edit", "experimental")
        read_tool = _tool("Read", "experimental")
        assert edit_tool.effective_llm_name == "HashlineEdit"
        assert read_tool.effective_llm_name == "HashlineRead"
        assert edit_tool.namespace == "experimental"

    def test_llm_names_do_not_collide_with_main_tools(self):
        names = {
            _tool("Edit").effective_llm_name,
            _tool("Read").effective_llm_name,
            _tool("Edit", "experimental").effective_llm_name,
            _tool("Read", "experimental").effective_llm_name,
        }
        assert names == {"Edit", "Read", "HashlineEdit", "HashlineRead"}

    def test_edit_schema_is_single_string_param(self):
        schema = _tool("Edit", "experimental").to_openai()
        assert schema["function"]["name"] == "HashlineEdit"
        params = schema["function"]["parameters"]
        assert list(params["properties"]) == ["input"]
        assert params["required"] == ["input"]

    def test_read_schema_params(self):
        schema = _tool("Read", "experimental").to_openai()
        assert schema["function"]["name"] == "HashlineRead"
        params = schema["function"]["parameters"]
        assert list(params["properties"]) == ["path", "offset", "limit"]
        assert params["required"] == ["path"]


# ── HashlineRead ──────────────────────────────────────────────────────


class TestHashlineRead:
    @pytest.mark.asyncio
    async def test_header_carries_tag_and_numbered_lines(self, tmp_path: Path):
        p = tmp_path / "notes.txt"
        p.write_text("alpha\nbeta\n", encoding="utf-8")
        ctx = _StubCtx()
        result = await read(ctx, str(p))
        tag = content_tag("alpha\nbeta\n")
        assert result.startswith(f"[hashline: {p}#{tag} | lines 1-2/2 | utf-8 | mtime ")
        assert "1→alpha" in result
        assert "2→beta" in result

    @pytest.mark.asyncio
    async def test_offset_limit_and_truncation_trailer(self, tmp_path: Path):
        p = tmp_path / "f.txt"
        p.write_text(
            "\n".join(f"line {i}" for i in range(1, 8)) + "\n", encoding="utf-8"
        )
        ctx = _StubCtx()
        result = await read(ctx, str(p), offset=3, limit=2)
        assert "lines 3-4/7" in result
        assert "3→line 3" in result and "4→line 4" in result
        assert "[... 3 more lines]" in result

    @pytest.mark.asyncio
    async def test_empty_and_missing_and_binary(self, tmp_path: Path):
        ctx = _StubCtx()
        empty = tmp_path / "empty.txt"
        empty.write_text("", encoding="utf-8")
        result = await read(ctx, str(empty))
        assert " | empty | " in result
        assert f"#{content_tag('')}" in result

        with pytest.raises(ToolError, match="No such file"):
            await read(ctx, str(tmp_path / "nope.txt"))

        binary = tmp_path / "bin"
        binary.write_bytes(b"\x00\x01\x02")
        with pytest.raises(ToolError, match="binary file"):
            await read(ctx, str(binary))

    @pytest.mark.asyncio
    async def test_bom_is_stripped_from_tag(self, tmp_path: Path):
        ctx = _StubCtx()
        plain = tmp_path / "plain.txt"
        plain.write_text("hi\n", encoding="utf-8")
        bommed = tmp_path / "bom.txt"
        bommed.write_bytes(b"\xef\xbb\xbfhi\n")
        assert f"#{content_tag('hi\n')}" in await read(ctx, str(plain))
        assert f"#{content_tag('hi\n')}" in await read(ctx, str(bommed))


# ── HashlineEdit ──────────────────────────────────────────────────────


class TestHashlineEdit:
    @pytest.mark.asyncio
    async def test_applies_patch_returns_new_tag_and_emits_window(self, tmp_path: Path):
        p = tmp_path / "notes.txt"
        p.write_text("alpha\nbeta\ngamma\n", encoding="utf-8")
        ctx = _StubCtx()
        old_tag = content_tag("alpha\nbeta\ngamma\n")

        result = await edit(ctx, f"[{p}#{old_tag}]\nPUT 2.=2:\n+BETA\n")

        new_tag = content_tag("alpha\nBETA\ngamma\n")
        assert result == (
            f"hashline: ok (1 file)\n  {p}: #{new_tag}, 1 op, 3 → 3 lines"
        )
        assert p.read_text(encoding="utf-8") == "alpha\nBETA\ngamma\n"
        (event,) = ctx.events
        assert event.type == "diff_content"
        assert event.path == str(p)
        assert event.old_text == "alpha\nbeta\ngamma"
        assert event.new_text == "alpha\nBETA\ngamma"
        assert (event.old_start_line, event.new_start_line) == (1, 1)

    @pytest.mark.asyncio
    async def test_stale_tag_rejects_with_zero_writes(self, tmp_path: Path):
        p = tmp_path / "notes.txt"
        p.write_text("alpha\n", encoding="utf-8")
        ctx = _StubCtx()
        stale = content_tag("something else\n")

        with pytest.raises(ToolError, match="stale tag"):
            await edit(ctx, f"[{p}#{stale}]\nPUT 1.=1:\n+x\n")

        assert p.read_text(encoding="utf-8") == "alpha\n"
        assert ctx.events == []

    @pytest.mark.asyncio
    async def test_any_section_failure_writes_nothing(self, tmp_path: Path):
        good = tmp_path / "good.txt"
        good.write_text("a\n", encoding="utf-8")
        ctx = _StubCtx()
        tag = content_tag("a\n")

        # 第一段可应用、第二段越界：任何一段失败 → 全部不落盘、零事件。
        payload = f"[{good}#{tag}]\nPUT 1.=1:\n+x\n[{good}#{tag}]\nPUT 9.=9:\n+y\n"
        with pytest.raises(ToolError, match="out of range"):
            await edit(ctx, payload)

        assert good.read_text(encoding="utf-8") == "a\n"
        assert ctx.events == []

    @pytest.mark.asyncio
    async def test_missing_file_hints_write(self, tmp_path: Path):
        ctx = _StubCtx()
        with pytest.raises(ToolError, match="No such file"):
            await edit(ctx, "[new.txt#AAAA]\nREM\n")

    @pytest.mark.asyncio
    async def test_named_registers_persist_across_calls_per_session(
        self, tmp_path: Path
    ):
        p = tmp_path / "f.txt"
        p.write_text("keep\nrest\n", encoding="utf-8")
        ctx = _StubCtx(session_id="reg-session")

        tag = content_tag("keep\nrest\n")
        await edit(ctx, f"[{p}#{tag}]\nCUT 1.=1 @saved\n")
        assert p.read_text(encoding="utf-8") == "rest\n"

        tag = content_tag("rest\n")
        await edit(ctx, f"[{p}#{tag}]\nPUT >$ @saved\n")
        assert p.read_text(encoding="utf-8") == "rest\nkeep\n"

        # 另一会话看不到寄存器（会话级作用域）。
        other = _StubCtx(session_id="other-session")
        with pytest.raises(ToolError, match="unknown register @saved"):
            await edit(other, f"[{p}#{content_tag('rest\nkeep\n')}]\nPUT >$ @saved\n")

    @pytest.mark.asyncio
    async def test_bom_preserved_and_not_hashed(self, tmp_path: Path):
        p = tmp_path / "bom.txt"
        p.write_bytes(b"\xef\xbb\xbfalpha\n")
        ctx = _StubCtx()
        tag = content_tag("alpha\n")

        await edit(ctx, f"[{p}#{tag}]\nPUT 1.=1:\n+beta\n")

        assert p.read_bytes() == b"\xef\xbb\xbfbeta\n"

    @pytest.mark.asyncio
    async def test_rem_and_mv(self, tmp_path: Path):
        ctx = _StubCtx()
        victim = tmp_path / "gone.txt"
        victim.write_text("bye\n", encoding="utf-8")
        result = await edit(ctx, f"[{victim}#{content_tag('bye\n')}]\nREM\n")
        assert not victim.exists()
        assert result.endswith(f"  {victim}: removed")

        mover = tmp_path / "old.txt"
        mover.write_text("m\n", encoding="utf-8")
        dest = tmp_path / "new.txt"
        result = await edit(ctx, f"[{mover}#{content_tag('m\n')}]\nMV {dest}\n")
        assert not mover.exists()
        assert dest.read_text(encoding="utf-8") == "m\n"
        assert result.endswith(f"  {mover}: #{content_tag('m\n')}, moved to {dest}")

    @pytest.mark.asyncio
    async def test_parse_error_is_tool_error_with_prefix(self, tmp_path: Path):
        ctx = _StubCtx()
        with pytest.raises(ToolError, match=r"^hashline: "):
            await edit(ctx, "not a patch")

    # ── 审查加固（评审轮修复项的行为锁定） ────────────────────────────

    @pytest.mark.asyncio
    async def test_repeated_sections_for_same_file_merge(self, tmp_path: Path):
        a = tmp_path / "a.txt"
        a.write_text("a\nb\nc\n", encoding="utf-8")
        b = tmp_path / "b.txt"
        b.write_text("x\ny\n", encoding="utf-8")
        ctx = _StubCtx()
        tag_a = content_tag("a\nb\nc\n")
        tag_b = content_tag("x\ny\n")
        # a 的段不连续（a → b → a）：按首次出现顺序归并，ops 按载荷顺序拼接。
        payload = (
            f"[{a}#{tag_a}]\nPUT 1.=1:\n+A\n"
            f"[{b}#{tag_b}]\nPUT 1.=1:\n+X\n"
            f"[{a}#{tag_a}]\nPUT 3.=3:\n+C\n"
        )
        result = await edit(ctx, payload)
        assert result == (
            f"hashline: ok (2 files)\n"
            f"  {a}: #{content_tag('A\nb\nC\n')}, 2 ops, 3 → 3 lines\n"
            f"  {b}: #{content_tag('X\ny\n')}, 1 op, 2 → 2 lines"
        ), result
        assert a.read_text(encoding="utf-8") == "A\nb\nC\n"
        assert b.read_text(encoding="utf-8") == "X\ny\n"
        assert len(ctx.events) == 3  # 每个操作一条窗口

    @pytest.mark.asyncio
    async def test_repeated_sections_with_different_tags_conflict(self, tmp_path: Path):
        p = tmp_path / "f.txt"
        p.write_text("a\n", encoding="utf-8")
        ctx = _StubCtx()
        tag = content_tag("a\n")
        other = "0000" if tag != "0000" else "FFFF"
        with pytest.raises(ToolError, match="different tags"):
            await edit(
                ctx, f"[{p}#{tag}]\nPUT 1.=1:\n+x\n[{p}#{other}]\nPUT 1.=1:\n+y\n"
            )
        assert p.read_text(encoding="utf-8") == "a\n"
        assert ctx.events == []

    @pytest.mark.asyncio
    async def test_registers_flow_across_sections_in_one_payload(self, tmp_path: Path):
        src = tmp_path / "src.txt"
        src.write_text("keep\n", encoding="utf-8")
        dst = tmp_path / "dst.txt"
        dst.write_text("base\n", encoding="utf-8")
        ctx = _StubCtx()
        payload = (
            f"[{src}#{content_tag('keep\n')}]\nCUT 1.=1 @moved\n"
            f"[{dst}#{content_tag('base\n')}]\nPUT >$ @moved\n"
        )
        await edit(ctx, payload)
        assert src.read_text(encoding="utf-8") == ""
        assert dst.read_text(encoding="utf-8") == "base\nkeep\n"

    @pytest.mark.asyncio
    async def test_anonymous_register_flows_across_sections(self, tmp_path: Path):
        src = tmp_path / "src.txt"
        src.write_text("keep\n", encoding="utf-8")
        dst = tmp_path / "dst.txt"
        dst.write_text("base\n", encoding="utf-8")
        ctx = _StubCtx()
        payload = (
            f"[{src}#{content_tag('keep\n')}]\nCUT 1.=1\n"
            f"[{dst}#{content_tag('base\n')}]\nPUT >$\n"
        )
        await edit(ctx, payload)
        assert dst.read_text(encoding="utf-8") == "base\nkeep\n"

    @pytest.mark.asyncio
    async def test_write_failure_rolls_back_and_keeps_registers_uncommitted(
        self, tmp_path: Path
    ):
        good = tmp_path / "good.txt"
        good.write_text("a\n", encoding="utf-8")
        bad = tmp_path / "bad.txt"
        bad.write_text("b\n", encoding="utf-8")
        bad.chmod(0o444)
        ctx = _StubCtx(session_id="rollback-session")

        payload = (
            f"[{good}#{content_tag('a\n')}]\nCUT 1.=1 @grab\nPUT >$:\n+after\n"
            f"[{bad}#{content_tag('b\n')}]\nPUT 1.=1:\n+x\n"
        )
        with pytest.raises(ToolError, match="nothing was changed"):
            await edit(ctx, payload)

        # 已落盘的 good 被回滚；bad 原样；零事件；捕获的寄存器未提交。
        assert good.read_text(encoding="utf-8") == "a\n"
        assert bad.read_text(encoding="utf-8") == "b\n"
        assert ctx.events == []
        with pytest.raises(ToolError, match="unknown register @grab"):
            await edit(ctx, f"[{good}#{content_tag('a\n')}]\nPUT >$ @grab\n")

    @pytest.mark.asyncio
    async def test_crlf_file_normalized_to_lf_on_edit(self, tmp_path: Path):
        p = tmp_path / "crlf.txt"
        p.write_bytes(b"a\r\nb\r\n")
        ctx = _StubCtx()

        result = await read(ctx, str(p))
        assert "\r" not in result
        tag = content_tag("a\nb\n")  # CRLF / LF 同一内容同一 tag
        assert f"#{tag}" in result

        await edit(ctx, f"[{p}#{tag}]\nPUT 2.=2:\n+B\n")
        assert p.read_bytes() == b"a\nB\n"

    @pytest.mark.asyncio
    async def test_mv_refuses_existing_destination_and_same_file(self, tmp_path: Path):
        src = tmp_path / "s.txt"
        src.write_text("s\n", encoding="utf-8")
        dst = tmp_path / "d.txt"
        dst.write_text("precious\n", encoding="utf-8")
        ctx = _StubCtx()

        with pytest.raises(ToolError, match="already exists"):
            await edit(ctx, f"[{src}#{content_tag('s\n')}]\nMV {dst}\n")
        assert src.read_text(encoding="utf-8") == "s\n"
        assert dst.read_text(encoding="utf-8") == "precious\n"

        with pytest.raises(ToolError, match="same file"):
            await edit(ctx, f"[{src}#{content_tag('s\n')}]\nMV {src}\n")
        assert src.read_text(encoding="utf-8") == "s\n"

    @pytest.mark.asyncio
    async def test_rem_and_mv_follow_symlink_write_through(self, tmp_path: Path):
        ctx = _StubCtx()

        real = tmp_path / "real.txt"
        real.write_text("r\n", encoding="utf-8")
        link = tmp_path / "link.txt"
        link.symlink_to(real)
        await edit(ctx, f"[{link}#{content_tag('r\n')}]\nREM\n")
        assert not real.exists()
        assert link.is_symlink()  # 链接本身保留（悬空）——写穿政策的删除侧

        real2 = tmp_path / "real2.txt"
        real2.write_text("r2\n", encoding="utf-8")
        link2 = tmp_path / "link2.txt"
        link2.symlink_to(real2)
        dest = tmp_path / "moved.txt"
        await edit(ctx, f"[{link2}#{content_tag('r2\n')}]\nMV {dest}\n")
        assert dest.read_text(encoding="utf-8") == "r2\n"
        assert not real2.exists()
        assert link2.is_symlink()
