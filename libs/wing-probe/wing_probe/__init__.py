"""wing-probe —— 确定性集成测试基础设施。

三角色：假 Provider（锚定模型，本批）、driver（假用户，round 4）、
observer（断言，round 4/5）。本批交付：包骨架 + import 门禁 + 环境自举 + 假 Provider。

典型用法（场景 fixture 内）：

    from wing_probe import ProbeEnv, Script, ToolCall, Turn

    env = await ProbeEnv.start(tmp_path)
    env.register("probe/basic", Script(Turn.of(text="done")))

硬约束：本包不得 ``import wing``（门禁 ``wing_probe.guard`` 在每次测试运行强制）。

契约：``openspec/changes/wing-probe/design.md``（D1–D12）与
``specs/probe-harness/spec.md``。
"""

from wing_probe.env import (
    DEFAULT_AGENT_TOOLS,
    DEFAULT_PROBE_MODEL,
    ProbeEnv,
    ProbeEnvError,
    candidate_gateway_binaries,
    log_section,
    read_log_tail,
    render_config_yaml,
    repo_root,
    reserve_port,
    resolve_gateway_bin,
    wait_for_health,
    write_config,
)
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
    "DEFAULT_AGENT_TOOLS",
    "DEFAULT_PROBE_MODEL",
    "DONE_TEXT",
    "PKG_ROOT",
    "SSE_CONTENT_TYPE",
    "ContextAssertionError",
    "ContextView",
    "FakeProvider",
    "LoggedRequest",
    "MessageView",
    "ProbeEnv",
    "ProbeEnvError",
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
    "candidate_gateway_binaries",
    "check_tree",
    "chunk_payload",
    "completion_response",
    "encode_turn_stream",
    "format_violations",
    "is_forbidden",
    "iter_python_files",
    "log_section",
    "match_message",
    "read_log_tail",
    "render_config_yaml",
    "repo_root",
    "reserve_port",
    "resolve_gateway_bin",
    "scan_file",
    "scan_source",
    "scan_tree",
    "stream_frames",
    "wait_for_health",
    "write_config",
]
