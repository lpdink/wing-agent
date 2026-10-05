# wing/provider/openai/__init__.py
"""wing.provider.openai 子包 — OpenAI 兼容协议实现。

稳定入口：``OpenAICompatProvider``（re-export）。实现分三个模块：

  - ``provider``：provider 对象（生命周期 / 公共 API / 状态 / 请求构造 / 流式泵）
  - ``serialize``：请求期消息序列化（Message 列表 → OpenAI 格式）
  - ``stream``：流状态与处理器（``_OAIStreamState`` / tool delta / 权威块数组）

实现细节直达子模块；包根只 re-export 稳定入口。
"""

from __future__ import annotations

from .provider import OpenAICompatProvider

__all__ = ["OpenAICompatProvider"]
