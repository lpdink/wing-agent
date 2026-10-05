# wing/provider/anthropic/__init__.py
"""wing.provider.anthropic 子包 — Anthropic Messages API 协议实现。

稳定入口：``AnthropicProvider``（re-export）。实现分三个模块：

  - ``provider``：provider 对象（生命周期 / 公共 API / 状态 / 请求构造 / 流式泵）
  - ``serialize``：请求期消息序列化（Message 列表 → Anthropic 格式）
  - ``stream``：流状态机与事件处理器（``_StreamState`` 与其 handler 同文件）

实现细节直达子模块；包根只 re-export 稳定入口。
"""

from __future__ import annotations

from .provider import AnthropicProvider

__all__ = ["AnthropicProvider"]
