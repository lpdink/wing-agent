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
