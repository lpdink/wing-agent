# wing/context/resources.py
"""skills / rules 资源加载 —— 与消息链无关的纯文件层。

从 ContextManager 分出（05_context 包化）：本模块不持有状态，输入是 glob
patterns 与解析基准（workspace），输出是 prompt 片段与展示信息。
ContextManager 保存结果快照（`_rule_files` / `_rules_prompt` / `_rules_files` /
`_skills_cache` / `_skills_prompt` / `_skill_sources`），`reload_skills_and_rules()`
时重新求值。

缓存账本需要的文件事实（`src` 的 path/mtime/size）也在这里采集——**加载时一次**
（`load_rule_files` 读取即 stat、`collect_skill_sources` 加载后 stat 一遍），
每次 LLM 调用的热路径不再碰文件系统。
"""

import glob
import os
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

import frontmatter

from wing.common.logger import log
from wing.schema import AgentSkill, CacheSource

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


def _resolve_patterns(patterns: list[str], workspace: Path | None) -> list[str]:
    """将 glob patterns 解析为实际路径列表。

    - ~ 路径: expanduser
    - 绝对路径: 保持不变
    - 相对路径: 基于 workspace 解析（无 workspace 则保持原样，由 glob 对 cwd 展开）
    """
    resolved: list[str] = []
    for p in patterns:
        expanded = os.path.expanduser(p)
        if workspace and not os.path.isabs(expanded):
            resolved.append(str(workspace / expanded))
        else:
            resolved.append(expanded)
    return resolved


def load_rules(
    rules_patterns: list[str], workspace: Path | None
) -> tuple[str, list[str]]:
    """加载所有 rules 文件并拼接；返回 (内容, 实际匹配并成功读取的文件路径)。

    rules 配置支持 glob 模式，如 ~/.wing/rules/*.md
    相对路径基于 workspace 解析。
    文件不存在或读取失败时 log.warning 并跳过。

    文件路径列表供 AgentInfo 下发，前端展示加载概览。
    """
    files = load_rule_files(rules_patterns, workspace)
    return rules_prompt(files)


def rules_prompt(files: list["RuleFile"]) -> tuple[str, list[str]]:
    """把已读到的 rules 文件拼成系统提示词片段；返回 (内容, 文件路径列表)。

    「路径 + 两换行 + 内容」逐文件拼接是**字节契约**（存量会话的请求前缀依赖它）——
    `load_rules` 与缓存账本的 `rules:<path>` 段都以此为唯一来源。
    """
    contents: list[str] = []
    for rule in files:
        contents.extend([rule.path, rule.content])
    return "\n\n".join(contents), [rule.path for rule in files]


@dataclass(frozen=True)
class RuleFile:
    """一个实际读到的 rules 文件（内容 + 加载时刻的文件事实）。

    `mtime` / `size` 在**读取的同一趟**采集：缓存账本的 `src` 元信息只能在这里拿一次，
    热路径（每次 LLM 调用）不得再去 stat（见 contract §1 的 `src` 约定）。
    """

    path: str
    content: str
    mtime: str | None = None
    size: int | None = None

    def source(self) -> CacheSource:
        return CacheSource(path=self.path, mtime=self.mtime, size=self.size)


def load_rule_files(
    rules_patterns: list[str], workspace: Path | None
) -> list[RuleFile]:
    """按加载顺序返回所有成功读取的 rules 文件（含加载时刻的 mtime / size）。

    读取顺序即 `load_rules` 拼进系统提示的顺序：pattern 序 × glob 序。`load_rules` 由本函数
    派生，两条路径的提示词字节因此不可能漂移。
    """
    all_patterns = _resolve_patterns(rules_patterns, workspace)
    if not all_patterns:
        return []

    files: list[RuleFile] = []
    for pattern in all_patterns:
        for file_path in sorted(glob.glob(pattern)):
            try:
                content = Path(file_path).read_text(encoding="utf-8")
            except Exception as e:
                log.warning(f"Failed to read rules file {file_path}: {e}")
                continue
            mtime, size = _file_facts(file_path)
            files.append(
                RuleFile(path=file_path, content=content, mtime=mtime, size=size)
            )
    return files


def collect_skill_sources(
    skills: dict[str, AgentSkill],
) -> dict[str, CacheSource]:
    """每个 skill 的来源事实（`src`），供缓存账本点名叫「哪份 SKILL.md 变了」。

    `path` 取 `AgentSkill.dir`（contract §1 规定），`mtime` / `size` 取该目录下的
    `SKILL.md`——决定这段提示词内容的就是它；文件缺失时退回目录自身的事实。

    只在**加载路径**调用一次（构造 / `reload_skills_and_rules`），每次调用都 stat 一遍
    的成本由此被摊平到"资源变化"的频率上，而不是每个 LLM 请求。
    """
    sources: dict[str, CacheSource] = {}
    for name, skill in skills.items():
        skill_md = os.path.join(skill.dir, "SKILL.md")
        target = skill_md if os.path.isfile(skill_md) else skill.dir
        mtime, size = _file_facts(target)
        sources[name] = CacheSource(path=skill.dir, mtime=mtime, size=size)
    return sources


def _file_facts(path: str) -> tuple[str | None, int | None]:
    """一次 stat 取事实（本地 naive ISO + 字节数）；失败返回 (None, None)。

    加载期的 stat 失败（权限 / 竞态删除）不该影响加载结果——账本的 `src` 是可选的
    展示素材，缺了它归因只是少几行细节。
    """
    try:
        st = os.stat(path)
    except OSError:
        return None, None
    return datetime.fromtimestamp(st.st_mtime).isoformat(), st.st_size


def load_all_skills(
    skills_patterns: list[str], workspace: Path | None
) -> dict[str, AgentSkill]:
    """从所有 skills glob patterns 加载技能。

    支持 * 单层目录匹配和 ** 多层目录匹配。
    相对路径基于 workspace 解析。
    每个匹配到的 .md 文件必须包含 name 和 description Front Matter。
    同名 skill 冲突处理：按 glob 结果排序后加载第一个，其他 log.warning 跳过。
    """
    all_skills: dict[str, AgentSkill] = {}
    seen_names: dict[str, str] = {}  # 记录已加载的 skill 名及其来源路径

    all_patterns = _resolve_patterns(skills_patterns, workspace)

    for pattern in all_patterns:
        matched_files = sorted(glob.glob(pattern, recursive=True))

        for skill_md_path in matched_files:
            skill_md = Path(skill_md_path)
            if not skill_md.is_file():
                continue

            skill_dir = skill_md.parent
            skill = _parse_skill(skill_dir, skill_md)
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


def _parse_skill(skill_dir: Path, skill_md: Path | None = None) -> AgentSkill | None:
    """解析单个 skill 目录的 SKILL.md 文件。"""
    if skill_md is None:
        skill_md = skill_dir / "SKILL.md"
    if not skill_md.is_file():
        log.warning(f"The skill directory '{skill_dir}' must include a SKILL.md file.")
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


def build_skills_prompt(skills_cache: dict[str, AgentSkill]) -> str:
    """构建 skills 提示词部分。"""
    if not skills_cache:
        return ""

    skill_descriptions = [
        _DEFAULT_AGENT_SKILL_INSTRUCTION,
    ] + [skill_block(skill) for skill in skills_cache.values()]
    return "\n".join(skill_descriptions)


def skill_block(skill: AgentSkill) -> str:
    """单个 skill 在 skills 提示块里的那一段（模板的唯一渲染处）。

    缓存账本的 `skills:<name>` 段文本取自这里——与 `build_skills_prompt` 拼进系统提示的
    字节逐字一致，`chars` 才能当代价估算用。
    """
    return _DEFAULT_AGENT_SKILL_TEMPLATE.format(
        name=skill.name,
        description=skill.description,
        dir=skill.dir,
    )


def render_skills_info(
    skills_patterns: list[str],
    skills_cache: dict[str, AgentSkill],
    rules_patterns: list[str],
    rules_files: list[str],
) -> str:
    """返回 skills/rules 信息，用于 /skills 命令显示。"""
    lines = []

    if skills_patterns:
        lines.append("📚 Skills patterns:")
        for pattern in skills_patterns:
            lines.append(f"  - {pattern}")
        lines.append("")

    if skills_cache:
        lines.append("已加载的 Skills:")
        for skill in skills_cache.values():
            lines.append(f"  {skill.name}: {skill.description}")
    else:
        lines.append("暂无已加载的 Skills")

    if rules_patterns:
        lines.append("")
        lines.append("Rules patterns:")
        for pattern in rules_patterns:
            lines.append(f"  - {pattern}")
        if rules_files:
            lines.append("已加载的 Rules 文件:")
            for file_path in rules_files:
                lines.append(f"  {file_path}")
        else:
            lines.append("暂无已加载的 Rules 文件")

    return "\n".join(lines)
