//! API 错误 → 退出码 + stderr 文案的唯一映射处。

use wing_api_client::ApiClientError;

// ============================================================
// 错误映射
// ============================================================

/// API 错误 →（退出码，stderr 文案；文案不含结尾换行）。纯函数：调用方负责打印，
/// 单测因此可以直接断言（不需要捕获进程流）。
pub(super) fn api_failure(error: &ApiClientError, http_base: &str, what: &str) -> (u8, String) {
    if is_unreachable(error) {
        return (
            2,
            format!("wing config error: 网关不可达（{http_base}）：先运行 wing start"),
        );
    }
    if error.is_conflict() {
        return (
            3,
            "wing config error: 配置已被其它客户端修改，请重试（或 --force 跳过指纹检查）"
                .to_string(),
        );
    }
    // 503 + error=="setup_mode"：settings 端点按 allowlist 不该被守门（见
    // `docs/dev/settings.md` 的 setup mode 一节），
    // 真出现说明网关状态异常——至少把"网关处于修复模式"这个事实告诉用户。
    if error.is_setup_mode() {
        return (
            1,
            format!(
                "wing config error: {what}：{error}\n  （网关处于修复模式：只有本机（loopback）客户端可以读写设置）"
            ),
        );
    }
    if let ApiClientError::Api { status: 404, .. } = error {
        return (
            1,
            format!(
                "wing config error: {what}：{error}\n  （该网关可能早于 Setting API；重启网关：wing stop && wing start）"
            ),
        );
    }
    (1, format!("wing config error: {what}：{error}"))
}

/// 传输层错误 = 连不上（`Connection` 是假后端 / tool host 用的同义变体）。
fn is_unreachable(error: &ApiClientError) -> bool {
    matches!(
        error,
        ApiClientError::Transport(_) | ApiClientError::Connection(_)
    )
}
