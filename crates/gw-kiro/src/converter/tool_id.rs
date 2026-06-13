//! 跨轮重复 `tool_use_id` 检测与重写。
//!
//! 🟢 借鉴 static_flow `converter/tool_name.rs::rewrite_duplicate_tool_use_ids`,但按对抗审查
//! 反馈做了两点收敛:**绝不报错**(畸形输入交给既有 [`super::pairing`] 兜底,不升级成 400)、
//! **种子纳入所有现存 id**(含 tool_result,防改写后缀复活孤儿)。
//!
//! 背景:Anthropic 报文里 assistant 轮含 `tool_use{id}`、随后 user 轮含 `tool_result{tool_use_id}`。
//! 客户端(尤其 Claude Code)反复 auto-compact 长对话后,同一 `tool_use_id` 可能在两个【各自
//! 已完成】的 assistant 轮里各出现一次。原 [`super::pairing::validate_tool_pairing`] 用 `HashSet`
//! 收集 tool_use_id 会静默去重,但 history 里仍残留两个同 id 的 `ToolUseEntry`,发给 Kiro 上游会
//! 400 Improperly formed。本模块把跨轮重复的 id(及其按 FIFO 配对的 tool_result)改写成带
//! `__caiodup{N}` 后缀的唯一 id,使每个 tool_use 与其 tool_result 一一对应。
//!
//! 设计要点:
//! - **只在我方发往 Kiro 的报文里改写**;模型新生成的 tool_use 由 Kiro 赋新 id、原样回传客户端,
//!   不需要回程还原(与 static_flow 保留 rewrite 映射不同——我方更轻)。
//! - **FIFO 配对**:同一 original_id 的多个 tool_use 按出现顺序排队,后续 tool_result 依次取队首
//!   对应的 normalized id。无法配对的残留(孤儿)留原值,交 pairing 阶段清理——本模块永不报错,
//!   绝不把可清理的畸形/compact 残留升级成用户可见 400。
//! - **确定性**:后缀按"该 original_id 第几次出现"分配(`__caiodup{occurrence}`),给定可见历史
//!   即稳定。⚠️ 已知局限:若客户端后续 compact 删掉了较早那次重复,同一逻辑调用的出现序号会变,
//!   wire id 随之变(`dup__caiodup2` → `dup`),打断该点之后的 Kiro 前缀缓存。这只影响【已含重复
//!   id 的罕见降级会话】,代价是一次缓存未命中(非 400),仍优于不改写直接 400——故接受不修。
//! - **调用时序**:必须在 conversationId 派生【之后】调用(见 mod.rs),身份哈希走原始 messages,
//!   与 worker 的 affinity_key_from_body(同基于原始 body)同源,改写不扰动会话身份/亲和/路由键。

use std::collections::{HashMap, HashSet, VecDeque};

use crate::anthropic_types::Message;

/// 检测并重写跨轮重复的 `tool_use_id`。
///
/// - 无重复 → `None`(零拷贝,常见路径)。
/// - 有重复 → `Some(rewritten)`(克隆并改写后的 owned messages)。
pub(super) fn rewrite_duplicate_tool_use_ids(messages: &[Message]) -> Option<Vec<Message>> {
    if !has_duplicate_tool_use_ids(messages) {
        return None;
    }
    let mut owned = messages.to_vec();
    apply_rewrite(&mut owned);
    Some(owned)
}

/// O(n) 预检:任一 assistant `tool_use` id 跨 block 出现两次即触发完整改写。
fn has_duplicate_tool_use_ids(messages: &[Message]) -> bool {
    let mut seen = HashSet::new();
    for message in messages {
        if message.role != "assistant" {
            continue;
        }
        let Some(items) = message.content.as_array() else {
            continue;
        };
        for item in items {
            if item.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }
            if let Some(id) = block_id(item, "id") {
                if !seen.insert(id) {
                    return true;
                }
            }
        }
    }
    false
}

fn apply_rewrite(messages: &mut [Message]) {
    // 种子:所有现存 id(tool_use 的 `id` + tool_result 的 `tool_use_id`)。后缀候选要避开它们
    // 全部——否则改写后的 id 可能撞上一个历史【孤儿 tool_result】的 id,把本该被 pairing 删除的
    // 孤儿"复活"成错误配对,仍可能 400(对抗审查 Skeptic#1)。
    let mut used_ids = collect_all_ids(messages);
    // original_id → 已见次数(含本次);第 N(N>1)次出现要改写。
    let mut seen_counts: HashMap<String, usize> = HashMap::new();
    // original_id → 等待配对的 normalized id 队列(FIFO)。后续 tool_result 依次取队首。
    let mut pending: HashMap<String, VecDeque<String>> = HashMap::new();

    for message in messages.iter_mut() {
        let assistant = message.role == "assistant";
        let user = message.role == "user";
        if !assistant && !user {
            continue;
        }
        let Some(items) = message.content.as_array_mut() else {
            continue;
        };

        if assistant {
            for item in items.iter_mut() {
                let Some(obj) = item.as_object_mut() else {
                    continue;
                };
                if obj.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                    continue;
                }
                let Some(original_id) = obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                else {
                    continue;
                };

                let count = seen_counts.entry(original_id.clone()).or_insert(0);
                *count += 1;
                let normalized = if *count == 1 {
                    original_id.clone()
                } else {
                    next_rewritten_id(&original_id, *count, &used_ids)
                };
                if normalized != original_id {
                    obj.insert(
                        "id".to_string(),
                        serde_json::Value::String(normalized.clone()),
                    );
                }
                used_ids.insert(normalized.clone());
                pending.entry(original_id).or_default().push_back(normalized);
            }
        } else {
            for item in items.iter_mut() {
                let Some(obj) = item.as_object_mut() else {
                    continue;
                };
                if obj.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                    continue;
                }
                let Some(original_id) = obj
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                else {
                    continue;
                };

                // FIFO:取该 original_id 最早一个待配的 normalized id;队空则留原值(孤儿,
                // 由 pairing 的 remove_orphaned_tool_results 清理),不报错。
                if let Some(normalized) = pending.get_mut(&original_id).and_then(VecDeque::pop_front)
                {
                    if normalized != original_id {
                        obj.insert(
                            "tool_use_id".to_string(),
                            serde_json::Value::String(normalized),
                        );
                    }
                }
            }
        }
    }
}

/// 收集报文中所有现存 id:assistant `tool_use.id` 与 user `tool_result.tool_use_id`。
fn collect_all_ids(messages: &[Message]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for message in messages {
        let Some(items) = message.content.as_array() else {
            continue;
        };
        for item in items {
            let id = match item.get("type").and_then(|v| v.as_str()) {
                Some("tool_use") => block_id(item, "id"),
                Some("tool_result") => block_id(item, "tool_use_id"),
                _ => None,
            };
            if let Some(id) = id {
                ids.insert(id);
            }
        }
    }
    ids
}

/// 取一个 block 指定字段的非空字符串(trim 后)。
fn block_id(item: &serde_json::Value, field: &str) -> Option<String> {
    item.get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn next_rewritten_id(original_id: &str, occurrence: usize, used_ids: &HashSet<String>) -> String {
    let mut suffix = occurrence;
    loop {
        let candidate = format!("{original_id}__caiodup{suffix}");
        if !used_ids.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: serde_json::Value) -> Message {
        Message {
            role: role.to_string(),
            content,
        }
    }

    fn tool_use(id: &str, name: &str) -> serde_json::Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": {}})
    }

    fn tool_result(id: &str, text: &str) -> serde_json::Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": text})
    }

    /// 取某条 message 第 idx 个 block 的某字段字符串。
    fn block_str(m: &Message, idx: usize, key: &str) -> String {
        m.content.as_array().unwrap()[idx]
            .get(key)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn no_duplicates_returns_none() {
        let messages = vec![
            msg("user", json!("hi")),
            msg("assistant", json!([tool_use("t1", "bash")])),
            msg("user", json!([tool_result("t1", "ok")])),
            msg("assistant", json!([tool_use("t2", "bash")])),
            msg("user", json!([tool_result("t2", "ok")])),
        ];
        assert!(rewrite_duplicate_tool_use_ids(&messages).is_none());
    }

    #[test]
    fn cross_turn_duplicate_renames_second_pair_only() {
        let messages = vec![
            msg("assistant", json!([tool_use("dup", "bash")])),
            msg("user", json!([tool_result("dup", "first")])),
            msg("assistant", json!([tool_use("dup", "bash")])),
            msg("user", json!([tool_result("dup", "second")])),
        ];
        let out = rewrite_duplicate_tool_use_ids(&messages).unwrap();
        // 第一对保持原 id。
        assert_eq!(block_str(&out[0], 0, "id"), "dup");
        assert_eq!(block_str(&out[1], 0, "tool_use_id"), "dup");
        // 第二对改写为带后缀的唯一 id,且 tool_use 与 tool_result 一致。
        let renamed = block_str(&out[2], 0, "id");
        assert_eq!(renamed, "dup__caiodup2");
        assert_eq!(block_str(&out[3], 0, "tool_use_id"), renamed);
    }

    #[test]
    fn same_turn_active_reuse_fifo_renames_no_error() {
        // 同一 assistant 轮里两个同 id tool_use(无中间 result):FIFO 排队,
        // 后续两个 result 按序配到 [d, d__caiodup2]。不报错(对抗审查 Skeptic#3/Minimalist#1)。
        let messages = vec![
            msg("assistant", json!([tool_use("d", "a"), tool_use("d", "b")])),
            msg("user", json!([tool_result("d", "r1"), tool_result("d", "r2")])),
        ];
        let out = rewrite_duplicate_tool_use_ids(&messages).unwrap();
        assert_eq!(block_str(&out[0], 0, "id"), "d");
        assert_eq!(block_str(&out[0], 1, "id"), "d__caiodup2");
        assert_eq!(block_str(&out[1], 0, "tool_use_id"), "d");
        assert_eq!(block_str(&out[1], 1, "tool_use_id"), "d__caiodup2");
    }

    #[test]
    fn three_occurrences_get_distinct_suffixes() {
        let messages = vec![
            msg("assistant", json!([tool_use("d", "a")])),
            msg("user", json!([tool_result("d", "1")])),
            msg("assistant", json!([tool_use("d", "a")])),
            msg("user", json!([tool_result("d", "2")])),
            msg("assistant", json!([tool_use("d", "a")])),
            msg("user", json!([tool_result("d", "3")])),
        ];
        let out = rewrite_duplicate_tool_use_ids(&messages).unwrap();
        assert_eq!(block_str(&out[0], 0, "id"), "d");
        assert_eq!(block_str(&out[2], 0, "id"), "d__caiodup2");
        assert_eq!(block_str(&out[4], 0, "id"), "d__caiodup3");
        assert_eq!(block_str(&out[1], 0, "tool_use_id"), "d");
        assert_eq!(block_str(&out[3], 0, "tool_use_id"), "d__caiodup2");
        assert_eq!(block_str(&out[5], 0, "tool_use_id"), "d__caiodup3");
    }

    #[test]
    fn parallel_distinct_ids_one_turn_not_touched() {
        let messages = vec![
            msg(
                "assistant",
                json!([tool_use("p1", "a"), tool_use("p2", "b")]),
            ),
            msg(
                "user",
                json!([tool_result("p1", "r1"), tool_result("p2", "r2")]),
            ),
        ];
        assert!(rewrite_duplicate_tool_use_ids(&messages).is_none());
    }

    #[test]
    fn suffix_avoids_colliding_with_existing_tool_use_id() {
        // 若 `d__caiodup2` 已被一个原生 tool_use 占用,后缀自增跳过。
        let messages = vec![
            msg("assistant", json!([tool_use("d", "a")])),
            msg("user", json!([tool_result("d", "1")])),
            msg("assistant", json!([tool_use("d", "a")])),
            msg("user", json!([tool_result("d", "2")])),
            msg("assistant", json!([tool_use("d__caiodup2", "a")])),
            msg("user", json!([tool_result("d__caiodup2", "x")])),
        ];
        let out = rewrite_duplicate_tool_use_ids(&messages).unwrap();
        let renamed = block_str(&out[2], 0, "id");
        assert_ne!(renamed, "d__caiodup2");
        assert_eq!(block_str(&out[3], 0, "tool_use_id"), renamed);
        assert_eq!(block_str(&out[4], 0, "id"), "d__caiodup2");
    }

    #[test]
    fn suffix_avoids_colliding_with_orphan_tool_result_id() {
        // 对抗审查 Skeptic#1:历史里有个【孤儿 tool_result】其 id 恰为 `d__caiodup2`。
        // 改写第二个 `d` 时必须避开它,否则会把孤儿复活成错误配对。
        let messages = vec![
            // 孤儿 tool_result(无对应 tool_use),id 恰好等于后缀候选。
            msg("user", json!([tool_result("d__caiodup2", "orphan")])),
            msg("assistant", json!([tool_use("d", "a")])),
            msg("user", json!([tool_result("d", "1")])),
            msg("assistant", json!([tool_use("d", "a")])),
            msg("user", json!([tool_result("d", "2")])),
        ];
        let out = rewrite_duplicate_tool_use_ids(&messages).unwrap();
        let renamed = block_str(&out[3], 0, "id");
        // 第二个 d 不能改成 d__caiodup2(会撞上孤儿)。
        assert_ne!(renamed, "d__caiodup2");
        assert_eq!(renamed, "d__caiodup3");
        assert_eq!(block_str(&out[4], 0, "tool_use_id"), renamed);
        // 孤儿原样保留(交 pairing 清理),未被错误配对。
        assert_eq!(block_str(&out[0], 0, "tool_use_id"), "d__caiodup2");
    }

    #[test]
    fn orphan_tool_result_left_unchanged() {
        // 跨轮重复触发改写,但某 tool_result 无任何对应 tool_use → 留原值给 pairing 清理。
        let messages = vec![
            msg("assistant", json!([tool_use("d", "a")])),
            msg("user", json!([tool_result("d", "1")])),
            msg("assistant", json!([tool_use("d", "a")])),
            msg(
                "user",
                json!([tool_result("d", "2"), tool_result("ghost", "x")]),
            ),
        ];
        let out = rewrite_duplicate_tool_use_ids(&messages).unwrap();
        // ghost 无对应 tool_use,保持原值。
        assert_eq!(block_str(&out[3], 1, "tool_use_id"), "ghost");
    }
}
