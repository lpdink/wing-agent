# wing/config/boot.py — 启动读取（永不抛）

"""``boot_config()`` —— 网关启动路径上**唯一**「读配置且不抛」的入口。

为什么单独成模块：

- 今天配置非法 ⇒ ``load_config()`` raise ⇒ 网关进程**直接退出**（总设计 §1.2 实测：
  最需要设置面板的那一刻，后端不存在）。降级启动（setup mode）需要一次永不抛的
  启动读取，把失败原因翻译成 ``ConfigProblem`` 列表交给设置面板 / CLI。
- ``loader.load_config()`` / ``get_config()`` 的对外行为**一字不改**（仍然抛）——
  现有调用方与测试零改动。``boot_config()`` 只被 ``gateway/cli.py`` 与
  ``GatewayServer`` 使用。
- 分层：本模块住 ``wing.config``（L3），不 import gateway / runtime。

四种结局（``BootFailure``）：文件缺席（写模板）· YAML 读不出（含顶层不是映射）·
校验不过 · 成功。**成功判定用 ``Config(**raw)``**（与 ``load_config`` 同一个构造），
所以「``boot.ok`` 为真」⟺ 「``load_config()`` 会成功」——这是后续
``GatewayServer`` 能放心进入正常模式的依据。
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Any

import yaml

from ..common.fs import atomic_write_text
from . import loader
from .catalog import build_catalog
from .document import (
    SparseDocument,
    locate_problems,
    malformed_document_problem,
    merge_with_defaults,
)
from .emit import default_document, emit_config_yaml
from .models import Config, GatewayConfig
from .problems import ConfigProblem, ProblemKind, cross_field_problems


class BootFailure(str, Enum):
    """``boot_config()`` 的结局分类（``ok=True`` 时为 ``None``）。

    ``str`` 混入让 ``boot.reason.value == "template_created"`` 这类比较与序列化
    口径稳定（CLI 横幅、日志、未来的诊断输出共用同一套词）。
    """

    MISSING_FILE = "missing_file"
    """文件不存在，且默认模板也写不出来（只读 home / 磁盘满）——问题清单带原始 OSError。"""

    PARSE_ERROR = "parse_error"
    """文件存在但读不出文档：YAML 语法错 / 顶层不是映射 / 读盘失败。"""

    INVALID = "invalid"
    """能解析成映射，但校验不过（字段级或跨字段）——问题清单与设置面板同一口径。"""

    TEMPLATE_CREATED = "template_created"
    """首次运行：文件不存在，刚写入默认模板（providers/agents 空 ⇒ 天然的 problem）。"""


@dataclass(frozen=True)
class BootResult:
    """启动读取的结果（**永不抛**）。

    ``problems`` 只在 ``ok=False`` 时非空（``ok=True`` 时恒为 ``[]``）；
    ``reason`` 只在 ``ok=False`` 时非 ``None``（四个取值全部描述失败/降级——
    给成功路径硬塞一个会撒谎）。
    """

    ok: bool
    config: Config | None
    problems: list[ConfigProblem]
    reason: BootFailure | None
    path: Path
    endpoint: tuple[str, int] | None
    """文件里的 ``gateway.host`` / ``gateway.port``（**只要文件能解析出映射就取**，
    各自类型合法时）。配置语义不合法不影响它——Rust 侧 ``cmd/backend_config.rs`` 也是
    ``#[serde(default)]`` 的部分结构，两边必须读到同一个 endpoint，
    否则「TUI 连不上降级启动的网关」（总设计 §8.6 末段）。取不到 ⇒ ``None``（回落默认）。"""


def boot_config() -> BootResult:
    """读 ``config.yaml`` 并返回结局 —— **永不抛**。

    **「永不抛」靠最外层兜底实现**（AD12）：任何没想到的形态（非字符串顶层键、
    未来 pydantic 的行为变化……）都在 :func:`_read_boot_config` 之外被兜成一条
    可展示的文档级 problem，而不是让网关带 traceback 崩掉——后者正是本步骤要消灭的路径。

    单例优先：``loader`` 已加载（同一进程内已 boot 过 / 组合根注入）⇒ 直接以它为准，
    **不重新加载配置**（不写盘、不校验）——``get_config()`` 的真相就是那个单例，
    重复读盘只会引入「同一个进程里两份配置」的分叉。成功时把 ``Config`` 塞回单例，
    让 ``get_config()`` 照常工作。

    除「文件缺席时写默认模板」外**无副作用**：不建 runtime、不注册 hook、
    不改任何全局状态（除 loader 单例）。
    """
    path = loader.get_config_path()
    try:
        return _read_boot_config(path)
    except Exception as exc:  # noqa: BLE001 — 兜底是本函数的契约（永不抛）
        return BootResult(
            ok=False,
            config=None,
            problems=[malformed_document_problem(exc)],
            reason=BootFailure.INVALID,
            path=path,
            endpoint=None,
        )


def _read_boot_config(path: Path) -> BootResult:
    """启动读取的主体（``boot_config()`` 的兜底之外的一切）。"""
    if loader._config is not None:
        return BootResult(
            ok=True,
            config=loader._config,
            problems=[],
            reason=None,
            path=path,
            endpoint=_endpoint_from_file(path),
        )

    if not path.exists():
        try:
            atomic_write_text(
                path, emit_config_yaml(default_document(), build_catalog())
            )
        except OSError as exc:
            return BootResult(
                ok=False,
                config=None,
                problems=[_problem(f"config.yaml 不存在，模板也写不出来：{exc}")],
                reason=BootFailure.MISSING_FILE,
                path=path,
                endpoint=None,
            )
        # 模板 = providers: [] / agents: []：天然的 problem（设置向导据此指路）。
        problems = _problems_of(SparseDocument(data=default_document()))
        return BootResult(
            ok=False,
            config=None,
            problems=problems,
            reason=BootFailure.TEMPLATE_CREATED,
            path=path,
            endpoint=None,
        )

    try:
        raw_bytes = path.read_bytes()
    except OSError as exc:
        return _failure(path, BootFailure.PARSE_ERROR, f"config.yaml 读不出来：{exc}")

    try:
        parsed = yaml.safe_load(raw_bytes.decode("utf-8"))
    except (yaml.YAMLError, UnicodeDecodeError) as exc:
        return _failure(path, BootFailure.PARSE_ERROR, _yaml_message(exc))

    if parsed is None:
        parsed = {}
    if not isinstance(parsed, dict):
        return _failure(
            path,
            BootFailure.PARSE_ERROR,
            f"config.yaml 顶层必须是映射，实得 {type(parsed).__name__}",
        )

    # 顶层键不是字符串（YAML 把裸数字 / bool / 日期解析成对应类型）：`Config(**parsed)`
    # 会抛 `TypeError: keywords must be strings`，而 pydantic 的 ValidationError
    # 路径根本进不去。先给一条**说得清**的问题，而不是让用户看 TypeError（AD12）。
    # endpoint 照常取——Rust 侧 serde 对结构体的非字符串键是**跳过**（实测：`1: oops`
    # 不影响它读到 `gateway.port`），两边必须落在同一个端口上。
    endpoint = _endpoint_of(parsed)
    bad_keys = [key for key in parsed if not isinstance(key, str)]
    if bad_keys:
        shown = ", ".join(str(key) for key in bad_keys[:5])
        more = "…" if len(bad_keys) > 5 else ""
        return _failure(
            path,
            BootFailure.INVALID,
            f"config.yaml 顶层键必须是字符串，实得：{shown}{more}",
            endpoint=endpoint,
        )

    try:
        config = Config(**parsed)
    except Exception:
        # 字段级 / 跨字段校验不过：问题清单与 GET /api/settings/status 同一口径。
        # 注意 unknown keys 不参与判定——Config 的 extra="ignore" 与 load_config 一致。
        return BootResult(
            ok=False,
            config=None,
            problems=_problems_of(SparseDocument(data=parsed)),
            reason=BootFailure.INVALID,
            path=path,
            endpoint=endpoint,
        )

    loader._config = config
    return BootResult(
        ok=True, config=config, problems=[], reason=None, path=path, endpoint=endpoint
    )


# ─────────────────────────────────────────────────────────────────────────────
# 内部
# ─────────────────────────────────────────────────────────────────────────────


def _problems_of(doc: SparseDocument) -> list[ConfigProblem]:
    """一份稀疏文档 → 全部问题（字段级 + 跨字段）。

    与 ``GET /api/settings/status`` 同一口径（那边的同名私有函数住
    ``gateway/routes/settings.py``）：L3 不许 import L4，所以「组合」这一行各写一次，
    底层实现（``locate_problems`` / ``cross_field_problems``）只有一份。
    """
    raw = merge_with_defaults(doc, build_catalog())
    return locate_problems(raw) + cross_field_problems(Config.model_construct(**raw))


def _endpoint_of(parsed: Any) -> tuple[str, int] | None:
    """从解析出的映射里取 ``gateway.host`` / ``gateway.port``。

    语义与 Rust 侧 ``BackendConfigFile`` 的 ``#[serde(default)]`` 对齐
    （``crates/wing/src/cmd/backend_config.rs``）：

    - 顶层解析不出映射 ⇒ ``None``（调用方回落默认；Rust 侧「整份解析失败」同价）；
    - ``gateway`` 段缺失 ⇒ 两字段都用声明默认值（Rust ``GatewaySection::default()``）；
    - 段里**某个字段类型不对** ⇒ **整段**回落默认值（Rust：serde 反序列化失败 →
      最外层 ``#[serde(default)]`` 整份回落；不是逐字段回落）；
    - 字段缺失（但存在的那些类型都对）⇒ 逐字段回落（Rust 的 ``#[serde(default)]``
      按字段生效）。
    """
    if not isinstance(parsed, dict):
        return None
    fallback = (_default("host", str), _default("port", int))
    gateway = parsed.get("gateway")
    if gateway is None:
        return fallback
    if not isinstance(gateway, dict):
        return fallback
    host = gateway.get("host", fallback[0])
    port = gateway.get("port", fallback[1])
    if not isinstance(host, str) or not host:
        return fallback
    if not isinstance(port, int) or isinstance(port, bool) or not 0 <= port <= 65535:
        return fallback
    return host, port


def _default(field: str, kind: type) -> Any:
    """``GatewayConfig`` 声明的默认值（与 ``DEFAULT_HOST`` / ``DEFAULT_PORT`` 同源）。

    声明层（01/02）是默认值的唯一来源——不在这里抄第三份 ``127.0.0.1`` / ``32523``。
    """
    value = GatewayConfig.model_fields[field].default
    return value if isinstance(value, kind) else kind()


def _endpoint_from_file(path: Path) -> tuple[str, int] | None:
    """只读地取 endpoint（文件不存在 / 读不出来 / 解析不了 ⇒ ``None``）——单例路径专用。"""
    try:
        raw = path.read_bytes()
    except OSError:
        return None
    try:
        parsed = yaml.safe_load(raw.decode("utf-8"))
    except (yaml.YAMLError, UnicodeDecodeError):
        return None
    return _endpoint_of(parsed)


def _yaml_message(exc: Exception) -> str:
    """YAML 读失败的文案 + **行号**（``problem_mark`` 是用户唯一能直接定位的信息）。"""
    mark = getattr(exc, "problem_mark", None)
    where = (
        f"（第 {mark.line + 1} 行，第 {mark.column + 1} 列）"
        if mark is not None
        else ""
    )
    return f"config.yaml 不是合法 YAML{where}：{exc}"


def _problem(message: str) -> ConfigProblem:
    """一条文档级 problem（``path=None``）——文案与 ``ConfigDocumentError.as_problem`` 同风格。"""
    return ConfigProblem(
        path=None,
        kind=ProblemKind.INVALID_VALUE,
        message=message,
        hint="修复 config.yaml（或从 config.yaml.bak 恢复）后重试",
    )


def _failure(
    path: Path,
    reason: BootFailure,
    message: str,
    *,
    endpoint: tuple[str, int] | None = None,
) -> BootResult:
    return BootResult(
        ok=False,
        config=None,
        problems=[_problem(message)],
        reason=reason,
        path=path,
        endpoint=endpoint,
    )
