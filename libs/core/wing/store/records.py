# wing/store/records.py
"""history 记录的规范形与标题回退——**只读 raw record** 的旁路规则。

落盘规范形（null 不写、assistant 的派生字段不写）与「会话标题回退」的取法都在
这里：调用方（fork 的原始 dict 复制、列表的标题）不经 pydantic 模型，所以这两条
规则与 ``Message`` 的模型边界同为落盘格式的守门人。
"""

from __future__ import annotations

from typing import Any, Iterator


IMAGE_ONLY_TITLE = "[image message]"
"""会话标题回退：链上只有纯图 user 消息（content 为空、内容全在 media）时的占位名。

没有它，这类会话的标题回退为空 → 上层（``SessionManager.list_sessions``）把
"无名且无标"的会话隐藏，用户刚建的贴图会话在列表里凭空消失（02 之前不可达：
纯图消息会被 ``run_turn`` 丢弃；之后是正常可达状态）。
"""


def first_user_title(records: Iterator[dict[str, Any]]) -> str | None:
    """会话标题回退：第一条**非空文本** user 记录（截断 100 字符）；没有文本但有
    携带 media 的 user 记录时回退为 ``IMAGE_ONLY_TITLE``；两者都没有 → None。

    跳过空 content 的 user 记录继续往后找——"图片先行"的会话不该因为第一条
    用户消息没有文本而在列表里无名。两个后端共用本函数（同一条规则只有一份实现）。
    """
    image_only = False
    for record in records:
        if record.get("role") != "user":
            continue
        text = str(record.get("content") or "")[:100]
        if text:
            return text
        image_only = image_only or bool(record.get("media"))
    return IMAGE_ONLY_TITLE if image_only else None


#: assistant 记录的派生字段：``content_blocks`` 的派生物，落盘恒不写
#: （``Message`` 的访问器实时计算，见 :func:`canonical_record`）。
_DERIVED_FLAT_FIELDS = ("content", "reasoning_content", "tool_calls")


def canonical_record(record: dict[str, Any]) -> dict[str, Any]:
    """history 记录的**规范形**——落盘前必过（规则唯一实现，新 dict 返回）。

    两条规则，都只服务"落盘只存事实不存副本"。

    - **null 不写**：缺席即默认。``0`` / ``False`` / ``""`` 照常写（显式建模，
      见链模型改造的硬约束）。走 ``model_dump`` 的调用方已用
      ``exclude_none=True`` 处理 model 字段（含嵌套）；这里的顶层过滤**主要
      服务 fork 的原始 dict 复制**（`session.fork._remap_record_uuids`，不经
      pydantic），顺带兜住任何要落盘的 dict。
    - **assistant 的派生字段不写**：``content`` / ``reasoning_content`` /
      ``tool_calls`` 由 ``content_blocks`` 派生。**只在该记录带 content_blocks
      时剥**——没有 blocks 的存量记录（develop 基线格式）里扁平字段是唯一
      存储，必须原样保留。

    自由载荷（工具参数 ``input`` / ``arguments``、``questions`` 等）里的 null
    是**数据**不是缺席，一律不动——它们不在 pydantic 模型字段上，天然不受
    ``exclude_none`` 影响；本函数只做顶层过滤，不下钻。

    读取侧的 ``Message`` 加载路径对两种形态都接受（``Message._route_flat``：
    blocks 在则扁平字段忽略，blocks 缺则由扁平值重建），因此**不需要迁移存量
    文件**。但**直接读 raw record 的辅助路径**（标题回退、fork 的 draft）要
    自己按 blocks 派生——它们看到的是记录，不是消息。
    """
    canonical = {key: value for key, value in record.items() if value is not None}
    if canonical.get("role") == "assistant" and "content_blocks" in canonical:
        for key in _DERIVED_FLAT_FIELDS:
            canonical.pop(key, None)
    return canonical
