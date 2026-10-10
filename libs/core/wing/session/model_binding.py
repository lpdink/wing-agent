# wing/session/model_binding.py
"""会话记录的**生效模型绑定**——resume 链的只读投影。

`Session._restore_persisted_model` 是「记录 → agent」的唯一**写**路径（resume /
逐出后水合时把模型三元组装回 agent）。只读面（`SessionManager.list_sessions`
的模型字段）走不了那条路——它不该创建 / 改动会话——但必须给出同一个答案：
「这个会话跑在哪个模型上」对用户是同一件事。本模块把那条优先级链做成纯函数：

1. ``model_id`` 命中 id 空间 → **当前映射**（``name`` / ``provider`` 跟随配置
   演化，id 不变而调用名改了是配置作者的正常操作）；
2. 未命中（id 被删 / 旧记录无 id）→ 记录里的快照 ``(provider_name, model_name)``
   （provider 仍可解析时；id 经 ``Config.identify`` 反查补全，可能仍为 None）；
3. 其余（无记录 / 记录不完整 / 快照 provider 不可用）→ **模板默认**。

两处链条必须一起改：只改一处就是「列表说的模型」与「resume 真跑的模型」的静默
分叉。应用侧的细节（日志 / 一次性落盘对齐 / 记录保留）以
`_restore_persisted_model` 为准，本模块只回答「是哪个模型」——无 IO、无副作用。
"""

from __future__ import annotations

from dataclasses import dataclass

from wing.config import Config, resolve_model_display_name
from wing.store import SessionMetadata

from .template import AgentTemplate


@dataclass(frozen=True)
class ModelBinding:
    """会话的生效模型：``model_id`` 是身份，``model_name`` / ``provider_name`` 是
    运行期事实，``model_display_name`` 是展示素材（未声明 = None）。

    与 ``/api/session/info`` 的模型四件套同源同刻：``model_id`` 是引用词（∈ 配置
    声明的 id 空间；陈旧数据下可为 None），``model_name`` 是发给上游的调用名，
    ``provider_name`` 是承载它的 provider。展示名不是身份——任何匹配仍以 id 为准。
    """

    model_id: str | None
    model_name: str
    provider_name: str
    model_display_name: str | None = None


def resolve_model_binding(
    metadata: SessionMetadata,
    config: Config,
    template: AgentTemplate,
) -> ModelBinding:
    """记录 → 生效模型绑定（优先级见模块 docstring）。

    ``template`` 是该会话的模板（``metadata.template_name`` 解析不到时的**默认**
    模板，口径同 :meth:`SessionManager.resume_session`）：它是第 3 级的答案。
    """
    if metadata.model_id is not None:
        ref = config.find_model(metadata.model_id)
        if ref is not None:
            return _binding(ref.id, ref.name, ref.provider_name, config)

    if (
        metadata.model_name
        and metadata.provider_name
        and _has_provider(config, metadata.provider_name)
    ):
        return _binding(
            config.identify(metadata.provider_name, metadata.model_name),
            metadata.model_name,
            metadata.provider_name,
            config,
        )

    return _binding(template.model_id, template.model, template.provider_name, config)


def display_name_of(provider_name: str, model_name: str, config: Config) -> str | None:
    """``(provider, 调用名)`` → 声明里的展示名；未声明 / provider 不可解析 = None。

    展示名的解析规则只有一份（配置侧 :func:`resolve_model_display_name`：按调用名
    查声明、空串即无、不做名字启发式），这里只负责把 provider 名解析成声明块。
    """
    try:
        provider_cfg = config.get_provider(provider_name)
    except ValueError:
        return None
    return resolve_model_display_name(provider_cfg, model_name)


def _binding(
    model_id: str | None,
    model_name: str,
    provider_name: str,
    config: Config,
) -> ModelBinding:
    return ModelBinding(
        model_id=model_id,
        model_name=model_name,
        provider_name=provider_name,
        model_display_name=display_name_of(provider_name, model_name, config),
    )


def _has_provider(config: Config, name: str) -> bool:
    try:
        config.get_provider(name)
    except ValueError:
        return False
    return True
