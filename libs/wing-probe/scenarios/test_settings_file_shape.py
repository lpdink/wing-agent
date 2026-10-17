"""``config.yaml`` 的形态契约：只有数据 + 两类注释（分隔行 / 注释掉的默认值）。

字段的 ``doc`` / ``notes`` / ``example`` 是**设置面板与 `wing config` 的素材**（同一个声明层，
经 ``GET /api/settings/schema`` 下发）；文件只承载数据与可发现性。把它们也写一份进
``config.yaml``，几十行的配置会膨胀成上千行（实测：一份 264 行数据的配置被写成 1217 行，
其中 938 行是散文），手动改配置的人看到的是噪声而不是指引。

本场景把这条产品口径钉在**真实保存路径**上（``POST /api/settings/set`` 的写盘产物就是
``emit.py`` 的输出）：

- **散文零残留**：schema 里所有 ``doc`` / ``notes`` / ``example`` / 分组说明，一个都不出现在文件里；
- **发现性不靠散文**：每个顶层键都以值行或注释行的形式出现；
- **注释只有两类**：``# ── Name ──…`` 分隔行，与 ``# key: …``（注释掉的默认值）；
- **注释掉的默认值与 schema 一致**（抽查三条），且整份文件的注释全部取消后仍是合法 YAML。
"""

from __future__ import annotations

import re
from collections.abc import Iterator
from typing import Any

import pytest
import yaml

from wing_probe import Probe

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
EMIT_MODEL = "probe/settings-emit"

#: 抽查：注释掉的默认值（路径 → 文件里必须出现的那一行）。三条覆盖三种标量形态
#: （int / bool / float），且都在 probe 配置里**没有**被写下来。
COMMENTED_DEFAULTS = {
    "images.max_bytes": "# max_bytes: 4718592",
    "sessions.eviction.enabled": "# enabled: true",
    "gateway.remote_tool_timeout": "# remote_tool_timeout: 1800.0",
}


def _walk(root: dict[str, Any]) -> Iterator[dict[str, Any]]:
    stack = [root]
    while stack:
        node = stack.pop()
        yield node
        stack.extend(node.get("children") or [])
        if node.get("element") is not None:
            stack.append(node["element"])
        stack.extend(node.get("variants") or [])


#: 文件里允许出现的注释形态（除此之外**任何**注释行都是散文泄漏）。
_ALLOWED_COMMENTS = (
    re.compile(r"^# wing-agent configuration — "),
    re.compile(r"^# ── .+ ─+$"),
    re.compile(r"^# [^\s:]+:(\s.*)?$"),  # 注释掉的默认值（`# key:` / `# key: value`）
    re.compile(r"^# unknown key \(not recognized by this wing version\)$"),
)


def _prose_of(schema: dict[str, Any]) -> list[str]:
    """schema 里所有给人读的文案（它们在文件里一个都不该出现）。"""
    prose: list[str] = []
    for node in _walk(schema["root"]):
        if node.get("doc"):
            prose.append(node["doc"])
        prose.extend(note for note in node.get("notes") or [] if note)
        if node.get("example"):
            prose.append(node["example"])
        if node.get("section_doc"):
            prose.append(node["section_doc"])
    prose.extend(group["doc"] for group in schema["groups"] if group.get("doc"))
    return prose


async def _settings(http: Any) -> dict:
    return await http.request("GET", "/api/settings/get")


@pytest.mark.probe_env(models=[EMIT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_saved_config_carries_no_prose_and_lists_every_key(probe: Probe) -> None:
    http = probe.driver_required.http
    schema = await http.request("GET", "/api/settings/schema")
    current = await _settings(http)
    document = current["values"]
    # 一次真实的改动（保存事务总会重写整份文件，这里只是让回执有意义）。
    document.setdefault("user_agent", {})["preset"] = "opencode"

    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["problems"] == [], receipt

    text = probe.env.config_path.read_text(encoding="utf-8")
    parsed = yaml.safe_load(text)
    assert parsed["user_agent"]["preset"] == "opencode", parsed

    lines = text.splitlines()
    comments = [line.lstrip() for line in lines if line.lstrip().startswith("#")]

    # ① 散文零残留：声明层的文案（数百条）一条都不许出现在注释里。
    #    只看注释行——数据行本来就该出现用户自己的字（`system_prompt` 里可能有任何文案）。
    prose = _prose_of(schema)
    assert len(prose) > 100, "schema 里读不到文案说明遍历坏了"
    leaked = [line for line in prose if any(line in c for c in comments)]
    assert leaked == [], leaked

    # ② 发现性不靠散文：每个顶层键都出现（有值 → 值行；用默认 → 注释行）。
    for child in schema["root"]["children"]:
        key = child["key"]
        present = any(re.match(rf"^\s*(# )?{re.escape(key)}:", line) for line in lines)
        assert present, (key, text)

    # ③ 注释只有白名单里的形态（文件头 / 分隔行 / 注释掉的默认值 / 未知键标记）。
    for line in comments:
        assert any(pattern.match(line) for pattern in _ALLOWED_COMMENTS), line

    # ④ 注释掉的默认值与 schema 一致。
    for path, expected in COMMENTED_DEFAULTS.items():
        assert any(line.strip() == expected for line in lines), (path, text)

    # ⑤ 「取消注释 = 一份合法 YAML」：除文件头与分隔行外，全部取消注释后仍能解析。
    def _uncommented(line: str) -> str | None:
        if line.startswith("# ") and ":" not in line:
            return None  # 文件头 / 分组分隔行（纯装饰，不进 YAML）
        return re.sub(r"^(\s*)# ", r"\1", line)

    uncommented = "\n".join(
        text for text in map(_uncommented, lines) if text is not None
    )
    pinned = yaml.safe_load(uncommented)
    assert isinstance(pinned, dict), pinned
    # 取消注释后默认值真的被钉住（抽查一条：images.max_bytes 从注释变成文档里的值）。
    assert pinned["images"]["max_bytes"] == 4718592, pinned["images"]
