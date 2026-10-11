"""skills / rules 资源加载：glob 解析、frontmatter 校验、冲突与降级。

这一层此前只测过「rules 匹配到文件」，skills 的加载（Agent Skills 的唯一入口）
完全没有覆盖。装配期约定：任何一个坏文件都只降级为告警 + 跳过，
MUST NOT 让 skills/rules 的加载失败拖垮会话创建。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.context.resources import (
    _DEFAULT_AGENT_SKILL_INSTRUCTION,
    _parse_skill,
    _resolve_patterns,
    build_skills_prompt,
    load_all_skills,
    load_rules,
    render_skills_info,
)


def _write_skill(root: Path, name: str, *, front: str | None = None) -> Path:
    """写一个 skill 目录，返回 SKILL.md 路径。front=None 表示省略 front matter。"""
    d = root / name
    d.mkdir(parents=True, exist_ok=True)
    md = d / "SKILL.md"
    if front is None:
        md.write_text(
            f"# {name}\n\nplain body without front matter\n", encoding="utf-8"
        )
    else:
        md.write_text(f"---\n{front}\n---\n\nbody\n", encoding="utf-8")
    return md


# ── _resolve_patterns ─────────────────────────────────────────


class TestResolvePatterns:
    def test_relative_patterns_resolve_against_workspace(self, tmp_path):
        assert _resolve_patterns(["rules/*.md"], tmp_path) == [
            str(tmp_path / "rules" / "*.md")
        ]

    def test_absolute_and_home_patterns_kept(self):
        home = str(Path.home() / ".wing" / "rules" / "*.md")
        assert _resolve_patterns(["~/.wing/rules/*.md"], None) == [home]
        assert _resolve_patterns(["/etc/wing/*.md"], Path("/ws")) == ["/etc/wing/*.md"]

    def test_without_workspace_relative_kept_as_is(self):
        # 无 workspace：交给 glob 对 cwd 展开
        assert _resolve_patterns(["rules/*.md"], None) == ["rules/*.md"]


# ── load_rules ────────────────────────────────────────────────


class TestLoadRules:
    def test_missing_pattern_yields_empty(self, tmp_path):
        assert load_rules([], tmp_path) == ("", [])

    def test_unreadable_match_is_skipped(self, tmp_path, wing_warnings):
        """glob 匹配到目录时读取失败：告警 + 跳过，其余文件照常拼接。"""
        (tmp_path / "rules").mkdir()
        (tmp_path / "rules" / "subdir").mkdir()  # 目录也算 glob 命中
        (tmp_path / "rules" / "a.md").write_text("rule A", encoding="utf-8")

        content, files = load_rules(["rules/*"], tmp_path)

        assert content == f"{tmp_path / 'rules' / 'a.md'}\n\nrule A"
        assert files == [str(tmp_path / "rules" / "a.md")]
        assert any("Failed to read rules file" in m for m in wing_warnings)

    def test_多文件按路径拼接(self, tmp_path):
        (tmp_path / "a.md").write_text("A", encoding="utf-8")
        (tmp_path / "b.md").write_text("B", encoding="utf-8")

        content, files = load_rules(["*.md"], tmp_path)

        assert files == [str(tmp_path / "a.md"), str(tmp_path / "b.md")]
        assert content.endswith("A\n\n" + str(tmp_path / "b.md") + "\n\nB")


# ── load_all_skills ───────────────────────────────────────────


class TestLoadSkills:
    def test_loads_skill_with_front_matter(self, tmp_path):
        _write_skill(
            tmp_path / "skills", "alpha", front="name: alpha\ndescription: first"
        )

        skills = load_all_skills(["skills/*/SKILL.md"], tmp_path)

        assert list(skills) == ["alpha"]
        assert skills["alpha"].description == "first"
        assert skills["alpha"].dir == str(tmp_path / "skills" / "alpha")

    def test_recursive_pattern(self, tmp_path):
        _write_skill(
            tmp_path / "a" / "deep", "beta", front="name: beta\ndescription: d"
        )

        skills = load_all_skills(["**/SKILL.md"], tmp_path)

        assert list(skills) == ["beta"]

    def test_directory_match_without_skill_md_is_skipped(self, tmp_path, wing_warnings):
        """pattern 直接命中目录（无 SKILL.md）：跳过，不产生条目也不报错。"""
        (tmp_path / "skills" / "empty").mkdir(parents=True)

        assert load_all_skills(["skills/*"], tmp_path) == {}
        assert wing_warnings == []

    def test_directory_match_is_skipped_silently(self, tmp_path, wing_warnings):
        """pattern 命中非文件（目录）时静默跳过——加载路径只认 .md 文件。"""
        (tmp_path / "skills" / "bare").mkdir(parents=True)

        assert load_all_skills(["skills/*"], tmp_path) == {}
        assert wing_warnings == []

    def test_missing_front_matter_fields_skipped(self, tmp_path, wing_warnings):
        _write_skill(tmp_path / "skills", "half", front="name: half")
        _write_skill(tmp_path / "skills", "nobody", front=None)

        assert load_all_skills(["skills/*/SKILL.md"], tmp_path) == {}
        assert len([m for m in wing_warnings if "YAML Front Matter" in m]) == 2

    def test_malformed_yaml_skipped(self, tmp_path, wing_warnings):
        md = tmp_path / "skills" / "broken" / "SKILL.md"
        md.parent.mkdir(parents=True)
        md.write_text("---\nname: [unclosed\n---\nbody\n", encoding="utf-8")

        assert load_all_skills(["skills/*/SKILL.md"], tmp_path) == {}
        assert any("Failed to parse SKILL.md" in m for m in wing_warnings)

    def test_duplicate_names_first_sorted_wins(self, tmp_path, wing_warnings):
        _write_skill(tmp_path / "skills", "aaa", front="name: dup\ndescription: first")
        _write_skill(tmp_path / "skills", "bbb", front="name: dup\ndescription: second")

        skills = load_all_skills(["skills/*/SKILL.md"], tmp_path)

        assert skills["dup"].description == "first"
        assert any("already loaded from" in m for m in wing_warnings)

    def test_parse_skill_defaults_to_skill_md(self, tmp_path):
        _write_skill(tmp_path / "skills", "gamma", front="name: gamma\ndescription: g")

        skill = _parse_skill(tmp_path / "skills" / "gamma")

        assert skill is not None and skill.name == "gamma"

    def test_parse_skill_missing_file_returns_none(self, tmp_path, wing_warnings):
        assert _parse_skill(tmp_path / "nope") is None
        assert any("must include a SKILL.md" in m for m in wing_warnings)


# ── prompt / 展示 ─────────────────────────────────────────────


class TestPromptAndInfo:
    def test_empty_cache_yields_empty_prompt(self):
        assert build_skills_prompt({}) == ""

    def test_prompt_lists_instruction_and_each_skill(self, tmp_path):
        from wing.schema import AgentSkill

        prompt = build_skills_prompt(
            {
                "alpha": AgentSkill(name="alpha", description="first", dir="/s/alpha"),
            }
        )

        assert prompt.startswith(_DEFAULT_AGENT_SKILL_INSTRUCTION)
        assert "## alpha" in prompt and "first" in prompt
        assert '"/s/alpha/SKILL.md"' in prompt

    def test_render_info_without_anything(self):
        info = render_skills_info([], {}, [], [])

        assert "暂无已加载的 Skills" in info
        assert "Skills patterns" not in info
        assert "Rules patterns" not in info

    def test_render_info_lists_patterns_and_rules(self):
        from wing.schema import AgentSkill

        info = render_skills_info(
            ["~/.wing/skills/*/SKILL.md"],
            {
                "alpha": AgentSkill(name="alpha", description="first", dir="/s/alpha"),
            },
            ["~/.wing/rules/*.md"],
            ["/s/rules/a.md"],
        )

        assert "📚 Skills patterns:" in info
        assert "  - ~/.wing/skills/*/SKILL.md" in info
        assert "alpha: first" in info
        assert "已加载的 Rules 文件:" in info
        assert "  /s/rules/a.md" in info

    def test_render_info_without_rules_files(self):
        info = render_skills_info([], {}, ["rules/*.md"], [])

        assert "暂无已加载的 Rules 文件" in info

    @pytest.mark.parametrize("value", ["alpha", 42, True])
    def test_front_matter_values_are_stringified(self, tmp_path, value):
        """frontmatter 里 name/description 不做类型校验——一律 str() 后入 schema。"""
        md = tmp_path / "s" / "SKILL.md"
        md.parent.mkdir(parents=True)
        md.write_text(
            f"---\nname: {value}\ndescription: {value}\n---\n", encoding="utf-8"
        )

        skills = load_all_skills(["s/SKILL.md"], tmp_path)

        assert list(skills) == [str(value)]
