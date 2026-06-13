//! 出站体积护栏：序列化请求体超上游上限时，从历史剔除最老媒体瘦身。
//!
//! 🔵 搬运自 kiro.rs v63。【实测定标 2026-06-13】Kiro 对序列化请求体存在总体积硬上限：
//! 全库实测 ≤6,341,854 字节全部成功、7,336,893 字节 112/112 确定性 400 "Improperly formed
//! request"（会话史携带 2 个 PDF + 8 张图，base64 共 ~6.4MB）。报文结构本身完全合法
//! （工具配对/占位兜底全过），唯一变量是体积。剔除历史媒体让会话继续可用，远好于整个
//! 会话被毒化（每轮必 400）。

use crate::kiro_types::conversation::Message;

/// 历史媒体被体积护栏剔除后的占位说明（追加到该轮 content 末尾）。
///
/// 文案刻意中性（不提 proxy/上游等基础设施细节）：它进入模型可见的对话内容，
/// 模型应把它当"附件不可用"的事实，而非复述基础设施状态。
const SHED_MEDIA_PLACEHOLDER: &str = "\n[attachment omitted due to size limits]";

/// 体积护栏剔除统计。
#[derive(Debug, Default, PartialEq)]
pub struct MediaShed {
    /// 剔除的文档数
    pub dropped_documents: usize,
    /// 剔除的图片数
    pub dropped_images: usize,
    /// 估算释放的字节数（base64 长度合计，未含 JSON 包装开销，偏保守）
    pub freed_bytes: usize,
}

/// 出站体积护栏：从 history **最老**的媒体开始按**单个附件**粒度剔除
/// （同轮内文档优先于图片：文档通常更大），直到估算释放量 ≥ `need_to_free`
/// 即停。被剔过附件的回合追加占位文本。**绝不动 currentMessage**（用户当轮意图）。
///
/// 设计取舍：
/// - 最老优先：越老的附件越可能已被早年轮次消化（assistant 的文字分析已留在史里），
///   剔除对模型可用信息损失最小；
/// - 附件粒度：只超限一点时不应把同轮所有附件全删光；
/// - 换占位而非静默删：模型/用户能看出附件曾存在但被移除，不会凭空幻觉；
/// - 剔空媒体后若该轮 content 也为空，占位文本天然满足 Kiro 非空约束；
/// - 只估算 base64 字节（不含 JSON 包装），实际释放量略大于估算，方向安全。
pub fn shed_history_media(history: &mut [Message], need_to_free: usize) -> MediaShed {
    let mut shed = MediaShed::default();
    for msg in history.iter_mut() {
        if shed.freed_bytes >= need_to_free {
            break;
        }
        let Message::User(user_msg) = msg else {
            continue;
        };
        let uim = &mut user_msg.user_input_message;
        let mut dropped_here = false;
        while shed.freed_bytes < need_to_free && !uim.documents.is_empty() {
            let d = uim.documents.remove(0);
            shed.freed_bytes += d.source.bytes.len();
            shed.dropped_documents += 1;
            dropped_here = true;
        }
        while shed.freed_bytes < need_to_free && !uim.images.is_empty() {
            let i = uim.images.remove(0);
            shed.freed_bytes += i.source.bytes.len();
            shed.dropped_images += 1;
            dropped_here = true;
        }
        if dropped_here {
            uim.content.push_str(SHED_MEDIA_PLACEHOLDER);
        }
    }
    if shed.dropped_documents + shed.dropped_images > 0 {
        tracing::warn!(
            "体积护栏：从历史剔除 {} 个文档 + {} 张图片，估算释放 {} 字节（目标 {}）",
            shed.dropped_documents,
            shed.dropped_images,
            shed.freed_bytes,
            need_to_free
        );
    }
    shed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro_types::conversation::{
        HistoryAssistantMessage, HistoryUserMessage, KiroDocument, KiroImage, UserMessage,
    };

    fn user_with_media(text: &str, docs: usize, imgs: usize, b64_len: usize) -> Message {
        let mut u = UserMessage::new(text, "claude-opus-4.8");
        for i in 0..docs {
            u.documents
                .push(KiroDocument::from_base64(format!("doc{i}"), "pdf", "A".repeat(b64_len)));
        }
        for _ in 0..imgs {
            u.images.push(KiroImage::from_base64("png", "B".repeat(b64_len)));
        }
        Message::User(HistoryUserMessage { user_input_message: u })
    }

    #[test]
    fn oldest_first_and_stops_when_enough() {
        let mut history = vec![
            user_with_media("turn1 pdf", 1, 0, 1000), // 最老，先剔
            Message::Assistant(HistoryAssistantMessage::new("ok")),
            user_with_media("turn2 img", 0, 1, 1000), // 够了就不该动它
        ];
        let shed = shed_history_media(&mut history, 500);
        assert_eq!(shed.dropped_documents, 1);
        assert_eq!(shed.dropped_images, 0, "释放够了不应继续剔");
        assert_eq!(shed.freed_bytes, 1000);
        if let Message::User(u) = &history[0] {
            assert!(u.user_input_message.documents.is_empty());
            assert!(u.user_input_message.content.contains("attachment omitted"));
        }
        if let Message::User(u) = &history[2] {
            assert_eq!(u.user_input_message.images.len(), 1, "未轮到的媒体不应动");
        }
    }

    #[test]
    fn per_attachment_granularity() {
        let mut u = UserMessage::new("mixed", "claude-opus-4.8");
        u.documents.push(KiroDocument::from_base64("d", "pdf", "A".repeat(1000)));
        u.images.push(KiroImage::from_base64("png", "B".repeat(1000)));
        u.images.push(KiroImage::from_base64("png", "C".repeat(1000)));
        let mut history = vec![Message::User(HistoryUserMessage { user_input_message: u })];
        let shed = shed_history_media(&mut history, 1500);
        assert_eq!(shed.dropped_documents, 1);
        assert_eq!(shed.dropped_images, 1, "达标即停，不删第 2 张");
        if let Message::User(u) = &history[0] {
            assert_eq!(u.user_input_message.images.len(), 1);
        }
    }

    #[test]
    fn no_media_noop() {
        let mut history = vec![Message::User(HistoryUserMessage::new("plain", "claude-opus-4.8"))];
        let shed = shed_history_media(&mut history, 10_000);
        assert_eq!(shed, MediaShed::default());
        if let Message::User(u) = &history[0] {
            assert_eq!(u.user_input_message.content, "plain", "无媒体 content 不应改");
        }
    }

    #[test]
    fn sheds_all_when_need_exceeds() {
        let mut history = vec![
            user_with_media("a", 1, 1, 100),
            user_with_media("b", 0, 2, 100),
        ];
        let shed = shed_history_media(&mut history, usize::MAX);
        assert_eq!(shed.dropped_documents, 1);
        assert_eq!(shed.dropped_images, 3);
        assert_eq!(shed.freed_bytes, 400);
    }
}
