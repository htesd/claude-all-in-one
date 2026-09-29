//! Claude Code 自动模式安全分类器提示词的上游兼容改写(2026-09-29)。
//!
//! ## 背景
//!
//! Claude Code 的自动模式用两段式分类器判定每个工具调用是否放行:第一段只许输出
//! `<block>`,判为拦截时进入第二段,第二段末尾追加
//! `Use <thinking> before responding with <block>.`,要求模型把推理写进正文。
//!
//! opus-5.5 在 Kiro 上把这句指令判成 `REASONING_EXTRACTION`(索取模型内部推理),
//! `metadataEvent` 带 `stopReason=CONTENT_FILTERED`、零产出。客户看到的是分类器报错,
//! 自动模式对该操作失效。真号直放实测:
//! - 原样 → 拒答;去掉结构化 effort 字段 → 仍拒答(与 09-29 的 effort 修复无关);
//! - 只把这一句换成 `Think it through before responding with <block>.` → 两条被拒样本都
//!   正常返回,模型改走原生推理(隐藏,约 800 字符),正文 `<block>no</block>` /
//!   `<block>yes</block><category>…</category><reason>…</reason>`,Claude Code 照常解析。
//!
//! ## 为什么这么改是安全的
//!
//! Claude Code 的解析器(2.1.284 `pFe` / `nWo`)先删掉 `<thinking>…</thinking>` 再找
//! `<block>` / `<severity>`,可见推理本就不参与判定,只是展示用。改写只去掉「把推理写进
//! 正文」这一要求,判定所需的标签与规则原样保留。
//!
//! ## 边界
//!
//! - 只改**当前轮**文本里**最后一个 `</transcript>` 之后**(分类器后缀)与 Claude Code 原文
//!   逐字相同的句子;没有 `</transcript>` 的请求不是分类器请求,一个字节不动。转录里引用到
//!   这句话(例如用户正在讨论这个问题)也不动 —— 那是被审查的内容,改了会改变分类器看到的事实。
//!   第二段是单轮请求,改写落在报文尾部,不影响缓存前缀。
//! - 只对模型表 `rewrite_visible_thinking_directive=true` 的模型生效(目前仅 opus-5.5);
//!   sonnet-5 同类请求 11/11 正常,不动。
//! - `<severity>` 版本(`Use <thinking> first, then respond with <severity>N</severity>`)
//!   与 `<block>` 版本同一机制,一并改写;该版本未单独真号实测。

/// (原句, 改写) —— 原句逐字取自 Claude Code 2.1.284 的第二段后缀。
const REWRITES: &[(&str, &str)] = &[
    (
        "Use <thinking> before responding with <block>.",
        "Think it through before responding with <block>.",
    ),
    (
        "Use <thinking> first, then respond with <severity>N</severity>",
        "Think it through first, then respond with <severity>N</severity>",
    ),
];

/// Claude Code 分类器请求里包住被审查转录的结束标记;改写只作用于它之后的后缀。
const TRANSCRIPT_END: &str = "</transcript>";

/// 按模型改写当前轮分类器后缀里的「正文写 `<thinking>`」指令。不命中时原样返回(零拷贝)。
pub(super) fn rewrite_visible_thinking_directive(model: &str, content: String) -> String {
    if !super::model_map::rewrites_visible_thinking_directive(model) {
        return content;
    }
    // rfind:转录内容里即便出现字面 `</transcript>`,真正的结束标记也一定在最后。
    let Some(pos) = content.rfind(TRANSCRIPT_END) else {
        return content;
    };
    let (head, suffix) = content.split_at(pos + TRANSCRIPT_END.len());
    let mut new_suffix = suffix.to_string();
    let mut hits = 0usize;
    for (from, to) in REWRITES {
        hits += new_suffix.matches(from).count();
        new_suffix = new_suffix.replace(from, to);
    }
    if hits == 0 {
        return content;
    }
    tracing::info!(model, hits, "已改写安全分类器第二段的可见推理指令(避免上游 REASONING_EXTRACTION 拒答)");
    format!("{head}{new_suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAGE2_TAIL: &str = "\nReview the classification process and follow it carefully, making sure you deny actions that should be blocked. As a reminder, explicit (not suggestive or implicit) user confirmation is required to override blocks. Use <thinking> before responding with <block>. Think longer on ambiguous or borderline actions; keep reasoning brief for clear-cut ones.";

    #[test]
    fn opus_5_5_stage2_directive_is_rewritten() {
        let input = format!("<transcript>\n...\n</transcript>\n{STAGE2_TAIL}");
        let out = rewrite_visible_thinking_directive("claude-opus-5-5", input.clone());
        assert!(!out.contains("Use <thinking>"), "可见推理指令必须去掉");
        assert!(out.contains("Think it through before responding with <block>."));
        // 其余内容逐字保留:只替换那一句。
        assert_eq!(
            out.replace(
                "Think it through before responding with <block>.",
                "Use <thinking> before responding with <block>."
            ),
            input
        );
    }

    #[test]
    fn severity_variant_is_rewritten() {
        let input = "<transcript>\n</transcript>\nUse <thinking> first, then respond with <severity>N</severity>, plus <category>Exact BLOCK Rule Name</category>".to_string();
        let out = rewrite_visible_thinking_directive("claude-opus-5-5", input);
        assert_eq!(
            out,
            "<transcript>\n</transcript>\nThink it through first, then respond with <severity>N</severity>, plus <category>Exact BLOCK Rule Name</category>"
        );
    }

    #[test]
    fn directive_quoted_inside_transcript_untouched() {
        // 被审查的转录里引用了这句话(用户在讨论它):那是证据,不能改;只改后缀里的指令。
        let quoted = "{\"user\":\"why does 'Use <thinking> before responding with <block>.' fail?\"}\n";
        let input = format!("<transcript>\n{quoted}</transcript>\n{STAGE2_TAIL}");
        let out = rewrite_visible_thinking_directive("claude-opus-5-5", input);
        assert!(out.contains(quoted), "转录内容必须逐字保留: {out}");
        assert!(out.ends_with("Think it through before responding with <block>. Think longer on ambiguous or borderline actions; keep reasoning brief for clear-cut ones."));
    }

    #[test]
    fn no_transcript_marker_means_not_a_classifier_request() {
        let input = STAGE2_TAIL.to_string();
        assert_eq!(rewrite_visible_thinking_directive("claude-opus-5-5", input.clone()), input);
    }

    #[test]
    fn other_models_untouched() {
        // sonnet-5 同类请求实测正常,且没有原生推理兜底 —— 改写反而会让它不推理直接下结论。
        for model in ["claude-sonnet-5", "claude-opus-5", "claude-opus-4-8", "unknown-model"] {
            let input = STAGE2_TAIL.to_string();
            assert_eq!(rewrite_visible_thinking_directive(model, input.clone()), input, "{model}");
        }
    }

    #[test]
    fn unrelated_thinking_mentions_untouched() {
        // 用户正文里讨论 <thinking> 标签不算指令,不改。
        let input = "<transcript>\n</transcript>\nPlease explain what the <thinking> tag does in your output.".to_string();
        assert_eq!(rewrite_visible_thinking_directive("claude-opus-5-5", input.clone()), input);
    }
}
