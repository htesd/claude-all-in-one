//! 块2d:正文内 `<thinking>` 标签解析(inline fallback)。
//!
//! 🔵 搬自 kiro.rs(已上线稳定)`anthropic/stream.rs` 的 `process_content_with_thinking`
//! 及 quote-aware 标签查找。**适用场景**:sonnet/haiku 等非 Opus 模型 thinking 不走独立
//! reasoningContentEvent 通道,而是把 `<thinking>...</thinking>` 混在 assistantResponseEvent
//! 正文里。本模块把它解析回 Anthropic 独立 thinking 块(否则标签裸奔进答案、污染输出)。
//!
//! 仅当 `thinking_enabled && !native_reasoning_seen` 时启用(见 chat.rs BlockTracker)。
//! 状态机逐 chunk 喂入,处理跨 chunk 半标签、引用字符包裹的伪标签、前导换行剥离。

/// 需要跳过的包裹字符:thinking 标签被这些字符紧邻包裹时视为"引用"而非真标签。
/// 🔵 与 kiro.rs 逐字节一致(保留逐字符数组形态,便于与原文比对,不收编成 byte str)。
#[allow(clippy::byte_char_slices)]
const QUOTE_CHARS: &[u8] = &[
    b'`', b'"', b'\'', b'\\', b'#', b'!', b'@', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'-',
    b'_', b'=', b'+', b'[', b']', b'{', b'}', b';', b':', b'<', b'>', b',', b'.', b'?', b'/',
];

fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer.as_bytes().get(pos).map(|c| QUOTE_CHARS.contains(c)).unwrap_or(false)
}

/// 查找真正的 `</thinking>` 结束标签(不被引用字符包裹,且后跟 `\n\n`)。
pub(super) fn find_real_thinking_end_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;
    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);
        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }
        let after_content = &buffer[after_pos..];
        if after_content.len() < 2 {
            return None; // 不足以判断 \n\n,等更多内容
        }
        if after_content.starts_with("\n\n") {
            return Some(absolute_pos);
        }
        search_start = absolute_pos + 1;
    }
    None
}

/// 查找真正的 `<thinking>` 开始标签(不被引用字符包裹)。
pub(super) fn find_real_thinking_start_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "<thinking>";
    let mut search_start = 0;
    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);
        if !has_quote_before && !has_quote_after {
            return Some(absolute_pos);
        }
        search_start = absolute_pos + 1;
    }
    None
}

/// 按 UTF-8 字符边界安全截取:返回 ≤ target 的最近边界。
pub(super) fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// inline `<thinking>` 解析产出的分段指令。chat.rs 据此发对应 SSE 事件
/// (text_delta / thinking 块 start+delta+signature+stop),复用 BlockTracker 的索引与签名逻辑。
#[derive(Debug, PartialEq)]
pub(super) enum InlineEvent {
    /// 正文文本(thinking 块之外)。
    Text(String),
    /// 进入 thinking 块(首次)。
    ThinkingStart,
    /// thinking 内容增量。
    ThinkingDelta(String),
    /// thinking 块结束(chat.rs 据此发 signature_delta + stop)。
    ThinkingEnd,
}

/// 正文内 `<thinking>` 流式解析状态机。
#[derive(Default)]
pub(super) struct InlineThinkingParser {
    buffer: String,
    in_thinking_block: bool,
    thinking_extracted: bool,
    strip_leading_newline: bool,
}

impl InlineThinkingParser {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// 喂入一段正文,返回有序分段指令。🔵 逻辑搬自 kiro.rs process_content_with_thinking。
    pub(super) fn feed(&mut self, content: &str) -> Vec<InlineEvent> {
        let mut out = Vec::new();
        self.buffer.push_str(content);
        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                if let Some(start_pos) = find_real_thinking_start_tag(&self.buffer) {
                    let before = self.buffer[..start_pos].to_string();
                    if !before.trim().is_empty() {
                        out.push(InlineEvent::Text(before));
                    }
                    self.in_thinking_block = true;
                    self.strip_leading_newline = true;
                    self.buffer = self.buffer[start_pos + "<thinking>".len()..].to_string();
                    out.push(InlineEvent::ThinkingStart);
                } else {
                    // 没找到开始标签:保留可能的半标签(<thinking> 长度),其余作为 text 发出。
                    let target = self.buffer.len().saturating_sub("<thinking>".len());
                    let safe = find_char_boundary(&self.buffer, target);
                    if safe > 0 {
                        let safe_content = self.buffer[..safe].to_string();
                        if !safe_content.trim().is_empty() {
                            out.push(InlineEvent::Text(safe_content));
                            self.buffer = self.buffer[safe..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 后紧跟的换行(可能跨 chunk)。
                if self.strip_leading_newline {
                    if self.buffer.starts_with('\n') {
                        self.buffer = self.buffer[1..].to_string();
                        self.strip_leading_newline = false;
                    } else if !self.buffer.is_empty() {
                        self.strip_leading_newline = false;
                    }
                }
                if let Some(end_pos) = find_real_thinking_end_tag(&self.buffer) {
                    let thinking_content = self.buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        out.push(InlineEvent::ThinkingDelta(thinking_content));
                    }
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    out.push(InlineEvent::ThinkingEnd);
                    self.buffer = self.buffer[end_pos + "</thinking>\n\n".len()..].to_string();
                } else {
                    // 未见结束标签:保留末尾 `</thinking>\n\n`(13B)长度,其余作 thinking_delta。
                    let target = self.buffer.len().saturating_sub("</thinking>\n\n".len());
                    let safe = find_char_boundary(&self.buffer, target);
                    if safe > 0 {
                        let safe_content = self.buffer[..safe].to_string();
                        if !safe_content.is_empty() {
                            out.push(InlineEvent::ThinkingDelta(safe_content));
                        }
                        self.buffer = self.buffer[safe..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完:剩余全部作为 text。
                if !self.buffer.is_empty() {
                    let remaining = std::mem::take(&mut self.buffer);
                    out.push(InlineEvent::Text(remaining));
                }
                break;
            }
        }
        out
    }

    /// 流结束:冲洗残留 buffer。未闭合的 thinking 块也要收尾(发剩余 delta + End)。
    pub(super) fn finish(&mut self) -> Vec<InlineEvent> {
        let mut out = Vec::new();
        if self.buffer.is_empty() {
            if self.in_thinking_block {
                self.in_thinking_block = false;
                out.push(InlineEvent::ThinkingEnd);
            }
            return out;
        }
        let remaining = std::mem::take(&mut self.buffer);
        if self.in_thinking_block {
            if !remaining.trim().is_empty() {
                out.push(InlineEvent::ThinkingDelta(remaining));
            }
            self.in_thinking_block = false;
            out.push(InlineEvent::ThinkingEnd);
        } else {
            out.push(InlineEvent::Text(remaining));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&str]) -> Vec<InlineEvent> {
        let mut p = InlineThinkingParser::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.feed(c));
        }
        out.extend(p.finish());
        out
    }

    #[test]
    fn whole_thinking_block_then_text() {
        let evs = collect(&["<thinking>\n推理内容</thinking>\n\n最终答案"]);
        assert_eq!(
            evs,
            vec![
                InlineEvent::ThinkingStart,
                InlineEvent::ThinkingDelta("推理内容".into()),
                InlineEvent::ThinkingEnd,
                InlineEvent::Text("最终答案".into()),
            ]
        );
    }

    #[test]
    fn thinking_split_across_chunks() {
        // 标签跨 chunk 分割:<think + ing> / </think + ing>\n\n
        let evs = collect(&["<think", "ing>\nabc", "def</think", "ing>\n\ntail"]);
        // 应正确识别开始/结束,thinking 内容 = abcdef,正文 = tail
        let thinking: String = evs.iter().filter_map(|e| match e {
            InlineEvent::ThinkingDelta(t) => Some(t.clone()),
            _ => None,
        }).collect();
        let text: String = evs.iter().filter_map(|e| match e {
            InlineEvent::Text(t) => Some(t.clone()),
            _ => None,
        }).collect();
        assert_eq!(thinking, "abcdef");
        assert_eq!(text, "tail");
        assert!(evs.contains(&InlineEvent::ThinkingStart));
        assert!(evs.contains(&InlineEvent::ThinkingEnd));
    }

    #[test]
    fn quoted_fake_tag_not_treated_as_real() {
        // 反引号包裹的 `<thinking>` 是引用,不应被当真标签
        let evs = collect(&["这里讲 `<thinking>` 标签的用法"]);
        // 全部应作为 text(无 ThinkingStart)
        assert!(!evs.iter().any(|e| matches!(e, InlineEvent::ThinkingStart)));
        let text: String = evs.iter().filter_map(|e| match e {
            InlineEvent::Text(t) => Some(t.clone()),
            _ => None,
        }).collect();
        assert!(text.contains("标签的用法"));
    }

    #[test]
    fn no_thinking_pure_text() {
        let evs = collect(&["就是一句普通回答,没有思考标签。"]);
        assert!(evs.iter().all(|e| matches!(e, InlineEvent::Text(_))));
    }

    #[test]
    fn unclosed_thinking_flushed_on_finish() {
        // 流结束时 thinking 未闭合:finish 应补 delta + End
        let evs = collect(&["<thinking>\n未闭合的推理"]);
        assert!(evs.contains(&InlineEvent::ThinkingStart));
        assert!(evs.iter().any(|e| matches!(e, InlineEvent::ThinkingDelta(_))));
        assert_eq!(evs.last(), Some(&InlineEvent::ThinkingEnd));
    }
}
