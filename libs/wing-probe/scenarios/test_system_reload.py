"""`/api/system/reload` 场景：逐项结果契约 + 记录开关重贴 + 配置/hook 真的重读。

reload 是"配置 / hook / provider 实例 / skills 全部重来一遍"的入口，重构（步骤
11 会把 `runtime.reload_system` 抽成模块）最容易改坏的正是三件事：

1. **provider 实例被换掉**：provider 级开关（thinking / reasoning_effort）随旧
   实例一起丢——不重贴就退回配置默认（`openai_compat` 构造时把
   `enable_thinking` / `preserve_thinking` setdefault 为 True），请求前缀漂移，
   且与 metadata 记录失配；
2. **config.yaml 是重读磁盘**：provider 级配置（如 `extra_body`）变更随
   `rebuild_providers` 生效——只刷新内存单例而不重建 client 的写法会静默失效；
3. **hook 是 clear + 重载**（不是叠加）：改写 hook 文件后 reload，旧 handler 必须
   消失——残留会让注入叠层。

覆盖的断言点：

- ``test_reload_reports_every_item_and_keeps_recorded_switches``：reload 响应的
  **名字序**恰好五项、逐项 ok、provider 项 detail 是确定计数（内存里只有本场景
  一个会话）；记录在案的 `thinking=False` / `reasoning_effort="high"` 在 reload
  后的下一次请求里逐字段一致（`reapply_provider_options` 生效），会话照常收尾，
  live 状态与记录一致；
- ``test_reload_rereads_provider_config_from_disk``：改盘上的 `config.yaml`
  （provider `extra_body` 加一个透传键）→ reload → 下一轮请求带上它（provider
  按新配置重建，而不是继续用旧实例）；
- ``test_reload_replaces_hook_registrations``：reload 前无 hook；写入 hook（标记 A）
  后 reload → 注入生效；把同一文件改写成标记 B 再 reload → 只有 B（A 不残留）。

环境旋钮：hook 场景用 ``@pytest.mark.probe_env(hooks=["hooks/*.py"])``（相对路径按
网关进程 cwd = ``env.root`` 解析），hook 文件由场景在 reload 前写入 ``<root>/hooks/``。

> 与 ``test_session_persistence.py::test_recorded_switches_survive_provider_rebuild``
> 的关系：那一轮守"重建后前缀身份"；本文件守 reload 的**响应契约**（逐项名字 /
  ok / 计数）、**config.yaml 重读**与 hook 的 `clear + 重载` 语义。三者互补，都不删。
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

import pytest
import yaml

from wing_probe import LoggedRequest, Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
RELOAD_MODEL = "probe/reload-switches"
CONFIG_MODEL = "probe/reload-config"
HOOK_MODEL = "probe/reload-hooks"

#: reload 响应的名字序（`runtime.reload_system` 的追加顺序 = 对外契约）。
EXPECTED_ITEMS = [
    "config.yaml",
    "hooks",
    "prompt commands",
    "provider",
    "skills & rules",
]

#: 请求体里影响服务端处理的开关（前缀身份的一部分）。
FLAG_KEYS = ("enable_thinking", "preserve_thinking", "reasoning_effort")

#: provider 配置透传键（改 config.yaml 后经 reload 生效的观测点）。
CONFIG_MARKER_KEY = "probe_config_marker"
CONFIG_MARKER_VALUE = "reloaded-from-disk"

#: hook 标记：唯一化，便于在请求体里精确搜索。
MARKER_A = "<probe-hook-a/>"
MARKER_B = "<probe-hook-b/>"

#: 场景写进 ``<env.root>/hooks/`` 的 hook 源（`before_user_message` 追加标记）。
#: ``__MARKER__`` 由 :func:`_install_hook` 替换（不用 ``str.format``——源码里的
#: f-string 花括号会被当成格式字段）。
HOOK_TEMPLATE = '''"""probe hook：before_user_message 追加标记。"""

from wing.hooks import hooks


@hooks.on("before_user_message")
def stamp(value, **context):
    return f"{value} __MARKER__"
'''

#: hook 文件名（两次 reload 用同一路径：替换语义才是断言对象）。
HOOK_FILE = "probe_hook.py"


def _install_hook(probe: Probe, marker: str) -> None:
    """把 hook 文件写进 ``<env.root>/hooks/``（reload 前调用）。"""
    hook_dir = probe.env.root / "hooks"
    hook_dir.mkdir(parents=True, exist_ok=True)
    (hook_dir / HOOK_FILE).write_text(
        HOOK_TEMPLATE.replace("__MARKER__", marker), encoding="utf-8"
    )


def _edit_config(probe: Probe, mutate: Callable[[dict[str, Any]], None]) -> None:
    """改盘上的 ``config.yaml``（load → mutate → dump），下一次 reload 才生效。"""
    config: dict[str, Any] = yaml.safe_load(
        probe.env.config_path.read_text(encoding="utf-8")
    )
    mutate(config)
    probe.env.config_path.write_text(
        yaml.safe_dump(config, sort_keys=False, allow_unicode=True), encoding="utf-8"
    )


def _flags(body: dict) -> dict:
    return {key: body.get(key) for key in FLAG_KEYS}


def _last_user_text(request: LoggedRequest) -> str:
    """请求里最后一条 user 消息的归一化文本（content 可能是块数组）。"""
    messages = request.context().messages
    assert messages and messages[-1].role == "user", request.describe()
    return messages[-1].content


def _assert_prefix_identity(
    before: LoggedRequest, after: LoggedRequest, *, shared: int
) -> None:
    """共享前缀逐项对账：system / tools 声明 / 处理开关 / 前 ``shared`` 条消息。"""
    assert after.body["messages"][0] == before.body["messages"][0], (
        "system 段必须逐字节一致"
    )
    assert after.body["tools"] == before.body["tools"], "tools 声明必须一致"
    assert _flags(after.body) == _flags(before.body), "处理开关必须一致"
    for position in range(shared):
        left = before.context().messages[position]
        right = after.context().messages[position]
        assert (left.role, left.content) == (right.role, right.content), (
            position,
            left.summary(),
            right.summary(),
        )


def _assert_reload_items(reload_result: dict, *, rebuilt_sessions: int) -> list[dict]:
    """reload 响应的逐项契约：名字序恰好五项、逐项 ok、provider 计数确定。"""
    assert reload_result["ok"] is True, reload_result
    items = reload_result["results"]
    assert [item["name"] for item in items] == EXPECTED_ITEMS, items
    assert [item["ok"] for item in items] == [True] * len(EXPECTED_ITEMS), items
    assert items[3].get("detail") == f"rebuilt {rebuilt_sessions} session(s)", items[3]
    return items


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_reload_reports_every_item_and_keeps_recorded_switches(
    probe: Probe,
) -> None:
    """reload 逐项 ok；记录在案的 provider 开关重贴，下一次请求前缀不漂移。"""
    probe.register(RELOAD_MODEL, Turn.of(text="reply one"), Turn.of(text="reply two"))
    session = await probe.session(model=RELOAD_MODEL)
    http = probe.driver_required.http

    # 显式动作：记录在案的 provider 级开关。默认请求体带 enable_thinking=true
    # （provider 构造时 setdefault），所以可观测的翻转是 true → false。
    await http.update_session(
        session.session_id, thinking=False, reasoning_effort="high"
    )

    result = await session.chat("alpha")
    assert result.data["subtype"] == "success", result.data
    before = probe.request(RELOAD_MODEL, 0)
    assert _flags(before.body) == {
        "enable_thinking": False,
        "preserve_thinking": True,
        "reasoning_effort": "high",
    }, before.body

    # 内存里只有本场景这一个会话 → provider 项计数确定。
    _assert_reload_items(await http.reload(), rebuilt_sessions=1)

    # 会话仍可继续对话：下一轮正常收尾。
    follow_up = await session.chat("beta")
    assert follow_up.data["subtype"] == "success", follow_up.data
    session.watch.assert_never("error")

    after = probe.request(RELOAD_MODEL, 1)
    # 记录重贴：新 provider 实例上的开关与 reload 前一致（否则退回配置默认
    # True——请求体字段翻转、前缀漂移）。
    assert _flags(after.body) == _flags(before.body), (before.body, after.body)
    # 前缀身份：system / tools / 旧消息逐项一致。
    _assert_prefix_identity(before, after, shared=len(before.context().messages))

    # live 状态与记录一致（不只是请求体凑对了）。
    info = await session.info()
    assert info["thinking"] is False, info
    assert info["reasoning_effort"] == "high", info


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_reload_rereads_provider_config_from_disk(probe: Probe) -> None:
    """config.yaml 的 provider 配置变更经 reload 生效（重读磁盘 + 重建 client）。"""
    probe.register(CONFIG_MODEL, Turn.of(text="reply one"), Turn.of(text="reply two"))
    session = await probe.session(model=CONFIG_MODEL)
    http = probe.driver_required.http

    await session.chat("alpha")
    before = probe.request(CONFIG_MODEL, 0)
    assert CONFIG_MARKER_KEY not in before.body, before.body

    def add_marker(config: dict[str, Any]) -> None:
        extra = config["providers"][0].setdefault("extra_body", {})
        extra[CONFIG_MARKER_KEY] = CONFIG_MARKER_VALUE

    _edit_config(probe, add_marker)
    _assert_reload_items(await http.reload(), rebuilt_sessions=1)

    await session.chat("beta")
    after = probe.request(CONFIG_MODEL, 1)
    # 新 provider 按新配置构建：透传键出现（旧实例会缺它）。
    assert after.body.get(CONFIG_MARKER_KEY) == CONFIG_MARKER_VALUE, after.body
    # 前缀身份不受影响（system / tools / 旧消息）。
    _assert_prefix_identity(before, after, shared=len(before.context().messages))


@pytest.mark.probe_env(
    hooks=["hooks/*.py"],
    # 不让 importlib 写 pyc：`load_hooks` 用 spec_from_file_location +
    # exec_module，字节码缓存按「mtime 取整到秒 + 文件大小」校验——场景在同一秒内
    # 等长改写 hook 文件，缓存判为有效就会读到旧代码（真实产品行为，见最终消息
    # 「观察到但未修」）。关掉写字节码后每次 exec 都真读源文件，替换语义可断言。
    env_overrides={"PYTHONDONTWRITEBYTECODE": "1"},
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_reload_replaces_hook_registrations(probe: Probe) -> None:
    """reload = hooks.clear() + 重载：改写 hook 文件后旧 handler 不残留。"""
    probe.register(
        HOOK_MODEL, Turn.of(text="r1"), Turn.of(text="r2"), Turn.of(text="r3")
    )
    session = await probe.session(model=HOOK_MODEL)
    http = probe.driver_required.http

    # 基线：hook 文件还不存在（glob 无匹配）——消息原样进请求。
    await session.chat("alpha")
    baseline = probe.request(HOOK_MODEL, 0)
    assert _last_user_text(baseline) == "alpha", baseline.body["messages"]

    # 写入 hook（标记 A）→ reload → 注入生效。
    _install_hook(probe, MARKER_A)
    _assert_reload_items(await http.reload(), rebuilt_sessions=1)
    await session.chat("beta")
    loaded = probe.request(HOOK_MODEL, 1)
    assert _last_user_text(loaded) == f"beta {MARKER_A}", loaded.body["messages"]

    # 改写同一文件（标记 B）→ 再 reload → 只有 B（旧 handler 已随 clear 摘除）。
    _install_hook(probe, MARKER_B)
    _assert_reload_items(await http.reload(), rebuilt_sessions=1)
    await session.chat("gamma")
    replaced = probe.request(HOOK_MODEL, 2)
    assert _last_user_text(replaced) == f"gamma {MARKER_B}", (
        "旧 handler 残留会叠成 'gamma <probe-hook-a/> <probe-hook-b/>'",
        replaced.body["messages"],
    )
    # 历史里的旧注入原样保留（hook 只作用于当轮入站消息，不追溯改写历史）。
    user_contents = [
        message.content
        for message in replaced.context().messages
        if message.role == "user"
    ]
    assert user_contents == ["alpha", f"beta {MARKER_A}", f"gamma {MARKER_B}"], (
        user_contents
    )
