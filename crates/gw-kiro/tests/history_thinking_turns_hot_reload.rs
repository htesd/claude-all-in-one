//! 历史 thinking 保留轮数「热改 → 下一次请求的 wire」全链路。
//!
//! **为什么必须是集成测试**:保留轮数存在进程级全局里。lib 单测跑在一个进程里、并行执行,
//! 任何一个用例把全局改成非 0 都会污染同进程其它用例(它们断言「历史 thinking 被剥离」) ——
//! 所以那边只测**显式传参**的纯函数层面。对抗审查在 default_thinking_effort 上指出过同款缺口:
//! 把 setter 里的赋值删掉,单测全绿而线上「面板改了不生效」。
//!
//! 集成测试各自是**独立的二进制与进程**,所以这里可以放心改全局。
//! ⚠️ 本文件内的用例共享同一个进程全局,必须**串行**跑 —— 靠 `SERIAL` 互斥锁保证,
//! 别在这里加不加锁就动全局的用例。

use std::sync::Mutex;

use gw_kiro::converter::convert_request;

static SERIAL: Mutex<()> = Mutex::new(());

/// 构造「历史 1 轮(assistant 带 thinking) + 当前轮」的最小请求,走 convert_request 全链路,
/// 返回发给 Kiro 的 history 里所有 assistant 消息的 content(含系统对的固定应答,在 index 0)。
fn history_assistant_contents() -> Vec<String> {
    let req: gw_kiro::anthropic_types::MessagesRequest =
        serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "问1"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "历史推理W"},
                    {"type": "text", "text": "历史答案V"}
                ]},
                {"role": "user", "content": "当前问"}
            ],
        }))
        .expect("构造请求");
    let result = convert_request(&req, "").expect("转换应成功");
    result
        .conversation_state
        .history
        .iter()
        .filter_map(|m| match m {
            gw_kiro::kiro_types::conversation::Message::Assistant(a) => {
                Some(a.assistant_response_message.content.clone())
            }
            _ => None,
        })
        .collect()
}

/// 历史里那条真实 assistant 消息的 content(index 0 是系统对的固定应答)。
fn history_answer() -> String {
    history_assistant_contents().remove(1)
}

/// worker 每 30s 喂进来的就是这个形状的 JSON:热改 → 下一次请求即生效,可逆。
#[test]
fn hot_changed_turns_reaches_the_wire_and_restores() {
    let _g = SERIAL.lock().unwrap();
    let provider =
        gw_kiro::KiroProvider::from_config(&serde_json::json!({}), reqwest::Client::new())
            .expect("构造 provider");

    // 出厂态(0 = 全丢):历史 thinking 被剥离(v49 以来的行为,上线默认值不改变行为)。
    assert_eq!(
        history_answer(),
        "历史答案V",
        "默认 0 时历史 thinking 应被剥离"
    );

    // 热改 turns=1:倒数最近 1 个 assistant 合并单元的 thinking 按确定性格式拼回。
    provider.apply_hot_settings(&serde_json::json!({"history_thinking_turns": 1}));
    assert_eq!(
        history_answer(),
        "<thinking>历史推理W</thinking>\n历史答案V",
        "热改 turns=1 后下一次请求就该带 thinking,不需要重启"
    );

    // 再热改回 0:立即恢复全丢(证明不是一次性生效,可逆)。
    provider.apply_hot_settings(&serde_json::json!({"history_thinking_turns": 0}));
    assert_eq!(history_answer(), "历史答案V", "改回 0 后应立即恢复剥离");
}

/// `apply_hot_settings` 这一层的接线纪律:缺字段不动当前值、非法类型只告警不生效。
#[test]
fn apply_hot_settings_missing_or_invalid_field_keeps_current_value() {
    let _g = SERIAL.lock().unwrap();
    let provider =
        gw_kiro::KiroProvider::from_config(&serde_json::json!({}), reqwest::Client::new())
            .expect("构造 provider");

    provider.apply_hot_settings(&serde_json::json!({"history_thinking_turns": -1}));
    assert_eq!(
        history_answer(),
        "<thinking>历史推理W</thinking>\n历史答案V",
        "turns=-1 应全量保留"
    );

    // 字段缺失 = 不动当前值(轮询响应偶发缺字段时不该悄悄打回出厂值)。
    provider.apply_hot_settings(&serde_json::json!({"cache_read_multiplier": 2.0}));
    assert!(
        history_answer().contains("历史推理W"),
        "缺字段不该重置保留轮数"
    );

    // 非法类型 = 只告警、不生效(手改 DB 可以绕过 admin 的 PUT 校验)。
    provider.apply_hot_settings(&serde_json::json!({"history_thinking_turns": "many"}));
    assert!(
        history_answer().contains("历史推理W"),
        "非整数不该改动生效值"
    );
    provider.apply_hot_settings(&serde_json::json!({"history_thinking_turns": 1.5}));
    assert!(history_answer().contains("历史推理W"), "浮点不该改动生效值");

    // 清 overlay 回基线(worker 每轮都用 from_effective 的全量值调用,删 overlay 即回 YAML 默认 0)。
    provider.apply_hot_settings(&serde_json::json!({"history_thinking_turns": 0}));
    assert_eq!(history_answer(), "历史答案V", "回 0 后应恢复剥离");
}
