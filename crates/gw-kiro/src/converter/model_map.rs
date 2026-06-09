//! 模型名映射与上下文窗口。

/// 模型映射：将 Anthropic 模型名映射到 Kiro 模型 ID
/// 严格对照版本号
pub fn map_model(model: &str) -> Option<String> {
    let model_lower = model.to_lowercase();

    if model_lower.contains("sonnet") {
        // 2026-05-29: Kiro 上游尚未支持 sonnet-4.8（INVALID_MODEL_ID），未来上线再加 4-8 分支
        if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-sonnet-4.6".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-sonnet-4.5".to_string())
        } else {
            None
        }
    } else if model_lower.contains("opus") {
        // 2026-05-29: opus-4.8 已可用（Kiro 上游确认）
        if model_lower.contains("4-8") || model_lower.contains("4.8") {
            Some("claude-opus-4.8".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-opus-4.5".to_string())
        } else if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-opus-4.6".to_string())
        } else if model_lower.contains("4-7") || model_lower.contains("4.7") {
            Some("claude-opus-4.7".to_string())
        } else {
            None
        }
    } else if model_lower.contains("haiku") {
        // haiku 历史一直兜底到 4.5（Kiro 暂无更新版本，含 4.8）
        Some("claude-haiku-4.5".to_string())
    } else {
        None
    }
}

/// 根据模型名称返回对应的上下文窗口大小
///
/// 复用 `map_model` 的映射逻辑，确保窗口大小判断与模型映射一致。
/// Kiro 于 2026-03-24 将 Opus 4.6 和 Sonnet 4.6 升级至 1M 上下文。
/// 4.7 同 1M
pub fn get_context_window_size(model: &str) -> i32 {
    match map_model(model) {
        Some(mapped) if mapped == "claude-sonnet-4.6"
            || mapped == "claude-opus-4.6"
            || mapped == "claude-opus-4.7"
            || mapped == "claude-opus-4.8" =>
        {
            1_000_000
        }
        _ => 200_000,
    }
}
