//! 文档名去重 + 净化（Bedrock 要求 document name 在整份请求内全局唯一且限定字符集）。
//!
//! 背景：Anthropic `{type:"document"}` 块可不带 name；[`super::content`] 把缺省名兜成
//! `"document"`。同一请求里多个无名（或同名）document → 全叫 `"document"` → Kiro 转发
//! Bedrock 时 400：
//!   `INVALID_DOCUMENT_NAME: Messages can't contain duplicate document names.`
//! 另外 Bedrock 的 document name 只允许「字母数字 / 空格 / 连字符 / 圆括号 / 方括号」，
//! 且不能出现连续多个空白；`report.pdf` 这类含点的名字也会被拒。
//!
//! 本模块在发往 Kiro 前，把所有 document 块的名字改写成「全局唯一 + 合法字符集」
//! （镜像 [`super::tool_id`]：零拷贝快路径、**绝不报错**、只作用于我方 wire 报文）。
//!
//! 时序：必须在 conversationId 派生【之后】调用（见 mod.rs），身份哈希走原始 messages，
//! 改写不扰动会话身份 / 亲和 / 路由键。已知局限同 tool_id：改名会打断该点之后的 Kiro
//! 前缀缓存，但这只在「一条消息带多份同名/无名附件」的罕见场景发生，代价是一次缓存
//! 未命中（非 400），优于直接 400，故接受不修。
//!
//! ⚠️ 去重后缀用**连字符**（`document-2`），不用 tool_id 的 `__caiodup` 下划线——
//! 下划线不在 Bedrock document name 允许集内。

use std::collections::HashSet;

use crate::anthropic_types::Message;

const DEFAULT_DOC_NAME: &str = "document";

/// Bedrock document name 长度上限 200 字符。基名钳到 192,给去重后缀 `-N` 留头寸,
/// 避免"名字太长"这一同类 INVALID_DOCUMENT_NAME 400 换个诱因复发(审查 MEDIUM#1)。
const MAX_DOC_NAME_LEN: usize = 192;

/// 检测并改写重复 / 非法的 document 名。无需改写 → `None`（零拷贝）。
/// 单文档、或名字本就全局唯一且合法时走零拷贝;多份无名/同名附件(正是触发本 bug 的场景)
/// 会命中 `messages.to_vec()` 克隆分支——一次性代价,改写幂等、跨轮确定性稳定,不会每轮重克隆。
pub(super) fn dedup_document_names(messages: &[Message]) -> Option<Vec<Message>> {
    if !needs_rewrite(messages) {
        return None;
    }
    let mut owned = messages.to_vec();
    apply(&mut owned);
    Some(owned)
}

fn item_is_document(item: &serde_json::Value) -> bool {
    item.get("type").and_then(|v| v.as_str()) == Some("document")
}

/// 文档块的「有效名」：显式非空 name 经净化后使用；缺省或净化后为空 → `"document"`。
///
/// ⚠️ 只读 `name`(与 [`super::content`] 发往 Bedrock 的取名口径严格一致)。Anthropic document
/// 块历史上也可能用 `title` 字段——但若这里读 title 而 content.rs 仍读 name,两边对"名字"的判断
/// 会不一致(dedup 以为两份不同 title 不撞名、content.rs 却都发缺省 "document" → 仍 400)。故要
/// 支持 title 必须【content.rs 与本模块一起改】+ 先抓真实客户端报文确认字段名,不能只在本模块加。
fn effective_name(item: &serde_json::Value) -> String {
    let raw = item
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty());
    match raw {
        Some(n) => {
            let s = sanitize(n);
            if s.is_empty() {
                DEFAULT_DOC_NAME.to_string()
            } else {
                s
            }
        }
        None => DEFAULT_DOC_NAME.to_string(),
    }
}

/// 是否需要改写：任一 document 名含非法字符（净化后与原始不同），或有效名在请求内撞名。
fn needs_rewrite(messages: &[Message]) -> bool {
    let mut seen: HashSet<String> = HashSet::new();
    for m in messages {
        let Some(items) = m.content.as_array() else {
            continue;
        };
        for item in items {
            if !item_is_document(item) {
                continue;
            }
            // 原始 name 含非法字符 → 需净化改写
            if let Some(raw) = item.get("name").and_then(|v| v.as_str()) {
                if sanitize(raw) != raw {
                    return true;
                }
            }
            // 有效名撞名 → 需去重改写
            if !seen.insert(effective_name(item)) {
                return true;
            }
        }
    }
    false
}

/// 就地把每个 document 块的 name 改写成「全局唯一 + 合法字符集」。
fn apply(messages: &mut [Message]) {
    let mut used: HashSet<String> = HashSet::new();
    for m in messages.iter_mut() {
        let Some(items) = m.content.as_array_mut() else {
            continue;
        };
        for item in items.iter_mut() {
            if !item_is_document(item) {
                continue;
            }
            let base = effective_name(item);
            let unique = uniquify(&base, &mut used);
            item["name"] = serde_json::Value::String(unique);
        }
    }
}

/// 若 base 未用过则直接用；否则依次尝试 `base-2` / `base-3` …（连字符合法）。
fn uniquify(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let mut n = 2usize;
    loop {
        let cand = format!("{base}-{n}");
        if used.insert(cand.clone()) {
            return cand;
        }
        n += 1;
    }
}

/// 净化到 Bedrock document name 允许集：字母数字 / 空格 / 连字符 / 圆括号 / 方括号。
/// 其它字符替换为连字符；压缩连续空白（Bedrock 禁止连续多个空白）；首尾空白去除。
fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_space = false;
    for ch in name.chars() {
        let keep = ch.is_ascii_alphanumeric()
            || ch == ' '
            || ch == '-'
            || ch == '('
            || ch == ')'
            || ch == '['
            || ch == ']';
        let c = if keep { ch } else { '-' };
        if c == ' ' {
            if prev_space {
                continue; // 压缩连续空白
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(c);
    }
    // 长度钳位:out 全为 ASCII(非法字符已替换成连字符),按字节截断即按字符截断,不会切碎多字节。
    out.truncate(MAX_DOC_NAME_LEN);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(messages: &[Message]) -> Vec<String> {
        let mut out = Vec::new();
        for m in messages {
            if let Some(items) = m.content.as_array() {
                for item in items {
                    if item_is_document(item) {
                        out.push(
                            item.get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("<none>")
                                .to_string(),
                        );
                    }
                }
            }
        }
        out
    }

    fn msgs(v: serde_json::Value) -> Vec<Message> {
        serde_json::from_value(v).expect("Message 反序列化失败（字段可能需按实际 struct 调整）")
    }

    fn doc(data: &str, name: Option<&str>) -> serde_json::Value {
        let mut d = serde_json::json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": data}
        });
        if let Some(n) = name {
            d["name"] = serde_json::Value::String(n.to_string());
        }
        d
    }

    #[test]
    fn two_unnamed_documents_get_unique_names() {
        let m = msgs(serde_json::json!([
            {"role": "user", "content": [doc("AAA", None), doc("BBB", None)]}
        ]));
        let out = dedup_document_names(&m).expect("应改写");
        assert_eq!(names(&out), vec!["document", "document-2"]);
    }

    #[test]
    fn duplicate_names_across_messages_deduped() {
        let m = msgs(serde_json::json!([
            {"role": "user", "content": [doc("AAA", Some("report"))]},
            {"role": "user", "content": [doc("BBB", Some("report"))]}
        ]));
        let out = dedup_document_names(&m).expect("应改写");
        assert_eq!(names(&out), vec!["report", "report-2"]);
    }

    #[test]
    fn illegal_chars_sanitized() {
        let m = msgs(serde_json::json!([
            {"role": "user", "content": [doc("AAA", Some("Q3 报告.pdf"))]}
        ]));
        let out = dedup_document_names(&m).expect("含非法字符应改写");
        // 中文与点被替换为连字符、连续连字符不特殊压缩（仅压缩空白），首尾 trim
        assert_eq!(names(&out).len(), 1);
        let got = &names(&out)[0];
        assert!(!got.contains('.'), "点应被净化: {got}");
        assert!(got.chars().all(|c| c.is_ascii_alphanumeric()
            || matches!(c, ' ' | '-' | '(' | ')' | '[' | ']')), "非法字符残留: {got}");
    }

    #[test]
    fn single_valid_unique_document_zero_copy() {
        let m = msgs(serde_json::json!([
            {"role": "user", "content": [doc("AAA", Some("report"))]}
        ]));
        assert!(dedup_document_names(&m).is_none(), "无需改写应返回 None（零拷贝）");
    }

    #[test]
    fn overlong_name_truncated_under_limit() {
        let long = "x".repeat(500);
        let m = msgs(serde_json::json!([
            {"role": "user", "content": [doc("AAA", Some(&long))]}
        ]));
        let out = dedup_document_names(&m).expect("超长名应触发截断改写");
        let got = &names(&out)[0];
        assert!(got.len() <= 200, "改写后仍超 Bedrock 200 上限: len={}", got.len());
        assert!(got.len() <= MAX_DOC_NAME_LEN, "基名应钳到 {MAX_DOC_NAME_LEN}");
    }

    #[test]
    fn no_documents_zero_copy() {
        let m = msgs(serde_json::json!([
            {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        ]));
        assert!(dedup_document_names(&m).is_none());
    }
}
