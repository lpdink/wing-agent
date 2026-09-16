"""wing-probe —— 确定性集成测试基础设施。

三角色：假 Provider（锚定模型）、driver（假用户）、observer（断言）。
本包是所有 probe 场景的地基，硬约束：不得 ``import wing``
（门禁 ``wing_probe.guard`` 在每次测试运行强制，见 design D2）。

契约：``openspec/changes/wing-probe/design.md``（D1–D12）与
``specs/probe-harness/spec.md``。
"""

from wing_probe.guard import (
    PKG_ROOT,
    Violation,
    check_tree,
    format_violations,
    is_forbidden,
    iter_python_files,
    scan_file,
    scan_source,
    scan_tree,
)
from wing_probe.provider import (
    DONE_TEXT,
    SSE_CONTENT_TYPE,
    ContextAssertionError,
    ContextView,
    FakeProvider,
    LoggedRequest,
    MessageView,
    RequestLog,
    SSEFrame,
    Script,
    ScriptError,
    ScriptExhaustedError,
    ScriptRegistry,
    ToolCall,
    ToolCallView,
    Turn,
    UnregisteredModelError,
    Usage,
    chunk_payload,
    completion_response,
    encode_turn_stream,
    match_message,
    stream_frames,
)

__all__ = [
    "DONE_TEXT",
    "PKG_ROOT",
    "SSE_CONTENT_TYPE",
    "ContextAssertionError",
    "ContextView",
    "FakeProvider",
    "LoggedRequest",
    "MessageView",
    "RequestLog",
    "SSEFrame",
    "Script",
    "ScriptError",
    "ScriptExhaustedError",
    "ScriptRegistry",
    "ToolCall",
    "ToolCallView",
    "Turn",
    "UnregisteredModelError",
    "Usage",
    "Violation",
    "check_tree",
    "chunk_payload",
    "completion_response",
    "encode_turn_stream",
    "format_violations",
    "is_forbidden",
    "iter_python_files",
    "match_message",
    "scan_file",
    "scan_source",
    "scan_tree",
    "stream_frames",
]
