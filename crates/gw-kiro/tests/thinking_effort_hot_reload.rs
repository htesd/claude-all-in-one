//! 默认思考档位「热改 → 下一次请求的 wire」全链路。
//!
//! **为什么必须是集成测试**:运行期档位存在进程级全局里。lib 单测跑在一个进程里、并发执行
//! 865 个用例,任何一个用例把全局改成别的值都会污染同进程的其它用例 —— 所以那边的用例刻意
//! 只把它设成当前值,只验校验语义。对抗审查三个 lens 一致指出这留下了缺口:
//! 把 `set_default_effort` 里的赋值删掉、或让 `normalize_effort` 回头读编译期常量,
//! **单测全绿而线上"面板改了不生效"**。
//!
//! 集成测试各自是**独立的二进制与进程**,所以这里可以放心改全局。
//! ⚠️ 本文件内的用例共享同一个进程全局,必须**串行**跑 —— 靠 `SERIAL` 互斥锁保证,
//! 别在这里加不加锁就动全局的用例。

use std::sync::Mutex;

use gw_kiro::anthropic_types::{
    default_effort, set_default_effort, MessagesRequest, OutputConfig, DEFAULT_EFFORT,
};
use gw_kiro::thinking_policy::{additional_model_request_fields, override_thinking_from_model_name};

static SERIAL: Mutex<()> = Mutex::new(());

fn req(model: &str) -> MessagesRequest {
    // 只带必填项:没有 thinking、没有 output_config —— 正是"客户端没说话"那条路径。
    serde_json::from_value(serde_json::json!({
        "model": model,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
    }))
    .expect("构造请求")
}

/// 走完整链路取出会发上 wire 的档位。
fn wire_effort(model: &str) -> Option<String> {
    let mut r = req(model);
    override_thinking_from_model_name(&mut r);
    let v = additional_model_request_fields(&r)?;
    Some(v["output_config"]["effort"].as_str()?.to_string())
}

#[test]
fn hot_changed_default_reaches_the_wire_and_restores() {
    let _g = SERIAL.lock().unwrap();

    // 出厂态:未热改时 wire 上是编译期兜底档。
    assert_eq!(wire_effort("claude-opus-5").as_deref(), Some(DEFAULT_EFFORT));

    // 热改到一个**不同**的档位 —— 这一步是单测覆盖不到的那一步。
    assert_eq!(set_default_effort("low"), Some("low"));
    assert_eq!(default_effort(), "low");
    assert_eq!(
        wire_effort("claude-opus-5").as_deref(),
        Some("low"),
        "热改后下一次请求就该带新档位,不需要重启"
    );

    // 再改一次,确认不是一次性生效。
    assert_eq!(set_default_effort("max"), Some("max"));
    assert_eq!(wire_effort("claude-opus-4-8").as_deref(), Some("max"));

    // 清 overlay 回基线(worker 每轮都用 from_effective 的全量值调用,删 overlay 即回 YAML 默认)。
    assert_eq!(set_default_effort(DEFAULT_EFFORT), Some(DEFAULT_EFFORT));
    assert_eq!(wire_effort("claude-opus-5").as_deref(), Some(DEFAULT_EFFORT));
}

/// 逐模型夹取必须**叠在**热改之后:4.6 系没有 `xhigh`,热改成 xhigh 后它要回落到自己的
/// schema default(high),而不是把一个上游 enum 里不存在的值发出去。
#[test]
fn hot_changed_default_still_goes_through_per_model_clamp() {
    let _g = SERIAL.lock().unwrap();

    assert_eq!(set_default_effort("xhigh"), Some("xhigh"));
    assert_eq!(wire_effort("claude-opus-5").as_deref(), Some("xhigh"), "opus-5 有 xhigh");
    assert_eq!(
        wire_effort("claude-opus-4-6").as_deref(),
        Some("high"),
        "4.6 无 xhigh，必须回落到它 schema 的 default，不能硬发 xhigh"
    );
    // 无 effort schema 的模型:热改到任何档位都**一个字段都不发**。
    assert_eq!(wire_effort("claude-opus-4-5"), None);
    assert_eq!(wire_effort("claude-haiku-4-5"), None);

    set_default_effort(DEFAULT_EFFORT);
}

/// 客户端**显式**点了档位时,热改的默认值不得盖掉它。
#[test]
fn hot_changed_default_never_overrides_an_explicit_client_effort() {
    let _g = SERIAL.lock().unwrap();

    assert_eq!(set_default_effort("low"), Some("low"));
    let mut r = req("claude-opus-5");
    r.output_config = Some(OutputConfig { effort: Some("max".into()), format: None });
    override_thinking_from_model_name(&mut r);
    let v = additional_model_request_fields(&r).expect("该字段必须发出");
    assert_eq!(v["output_config"]["effort"], "max", "客户端显式 max 不该被默认档压成 low");

    set_default_effort(DEFAULT_EFFORT);
}

/// `apply_hot_settings` 这一层的接线:worker 每 30s 喂进来的就是这个形状的 JSON。
/// 缺字段不动当前值、非法值不生效 —— 两条都要成立。
#[test]
fn apply_hot_settings_wires_the_field_through_the_provider() {
    let _g = SERIAL.lock().unwrap();

    let provider = gw_kiro::KiroProvider::from_config(&serde_json::json!({}), reqwest::Client::new())
        .expect("构造 provider");

    provider.apply_hot_settings(&serde_json::json!({"default_thinking_effort": "xhigh"}));
    assert_eq!(default_effort(), "xhigh", "provider 应把该字段接到运行期档位上");
    assert_eq!(wire_effort("claude-opus-5").as_deref(), Some("xhigh"));

    // 字段缺失 = 不动当前值(轮询响应偶发缺字段时不该悄悄打回出厂值)。
    provider.apply_hot_settings(&serde_json::json!({"cache_read_multiplier": 2.0}));
    assert_eq!(default_effort(), "xhigh", "缺字段不该重置档位");

    // 非法值 = 只告警、不生效(手改 DB 可绕过 admin 校验)。
    provider.apply_hot_settings(&serde_json::json!({"default_thinking_effort": "ludicrous"}));
    assert_eq!(default_effort(), "xhigh", "非法值不该改动生效档位");

    provider.apply_hot_settings(&serde_json::json!({"default_thinking_effort": DEFAULT_EFFORT}));
    assert_eq!(default_effort(), DEFAULT_EFFORT);
}
