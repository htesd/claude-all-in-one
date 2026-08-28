//! `redactedContent` 线缆形状抓样 —— **只观测,不处理**(2026-08-20 用户决策)。
//!
//! 【为什么需要】Kiro 的 `reasoningContentEvent` payload 可能携带 `redactedContent`
//! (Smithy `ReasoningContent` union 的另一支)。拆包 kiro.kiro-agent@1.0.369 里客户端
//! 确实从**响应侧**读 `reasoningRedactedContent`(见 `oe12` 与 `withReasoningContent`
//! 的第 4 个参数),所以协议上它是**预期输入**,不是死分支。
//! 我方 `chat.rs` 目前只实现 `text`/`signature` 那一支的下行。
//!
//! 【为什么不直接接线】Anthropic 文档只给了**非流式**块形状
//! `{"type":"redacted_thinking","data":".."}`,**从未描述它在 SSE 里的形状**
//! (整份 thinking 文档 7 处 redacted 提及,"Streaming thinking" 一节一次没提)。
//! ⚠️ 而且(对抗评审 r2 #5)**本抓样只能确定 Kiro eventstream 的形状,不能推出
//! Anthropic SSE 该发什么** —— 后者仍需官方 schema / SDK 类型或客户端兼容测试。
//! 本模块解决的是前一个未知:redacted 是否真会来、来时和明文/签名如何共存与排序。
//!
//! 【纪律】
//! - **默认开**:开关关着就抓不到,失去意义。判定成本 = 一次 `Value::get`,近零。
//! - **三重上限**(对抗评审 r2 #2):进程内条数、**文件总字节**(跨重启有效,计数器会清零
//!   但文件不会)、单条记录字节。任何一项到顶即停。诊断设施绝不允许把盘写满。
//! - **额度只由成功写入消耗**(对抗评审 r2 #4):否则前几次因目录/权限失败就会让进程永久失明。
//! - **绝不失败传播**:任何 IO/序列化错误只 `warn!`,不影响正在进行的流。
//! - ⚠️ 样本含**客户内容**,且**不保证只有密文**(payload 可能同时带明文 `text`)。
//!   落盘按 `0600` 建文件。仅供本机诊断:**不要发给任何第三方**(含 codex / kimi 评审),
//!   不要提交进仓库(`data/` 已 gitignore)。看完请 `shred -u` 删掉。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// 进程内最多**成功**抓多少条。形状是恒定的,不需要海量样本。
const MAX_SAMPLES: usize = 20;
/// 样本文件总字节上限。**这一条才是跨重启的真上限** —— 进程计数器会随重启清零,
/// 文件不会;没有它,长期反复重启可让文件无限增长(对抗评审 r2 #2)。
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// 单条落盘记录的字节上限(含 keys/field_lens/payload 全部字段)。
const MAX_RECORD_BYTES: usize = 64 * 1024;
/// `field_lens` / `keys` 最多记多少个字段 —— 上游 payload 键数未知,
/// 不设限的话畸形/超大帧能让**这两项自己**远超记录上限(对抗评审 r2 #2)。
const MAX_KEYS: usize = 32;
/// 单个字符串字段记录长度时的读取上限;更长只记这个封顶值,不做任何拷贝。
const MAX_FIELD_LEN_REPORTED: usize = 16 * 1024 * 1024;

/// 已**成功**落盘的条数。名字与语义一致(对抗评审 r2 #4:不再统计"尝试")。
static SUCCEEDED: AtomicUsize = AtomicUsize::new(0);
/// 是否已因触顶告警过(只告警一次,避免高频事件淹掉日志)。
static CAPPED_WARNED: AtomicUsize = AtomicUsize::new(0);

/// 落盘路径。默认 `data/kiro-redacted-samples.jsonl` —— 与 `data/control.db` 同目录,
/// docker-compose 里 `./data:/app/data` 是 bind mount,容器重建不丢、宿主机直接可读。
/// 可用 env `KIRO_REDACTED_SAMPLE_PATH` 覆盖。
fn sample_path() -> &'static str {
    static P: OnceLock<String> = OnceLock::new();
    P.get_or_init(|| {
        std::env::var("KIRO_REDACTED_SAMPLE_PATH")
            .unwrap_or_else(|_| "data/kiro-redacted-samples.jsonl".to_string())
    })
}

/// payload 是否携带 `redactedContent`(判定入口,调用方据此决定是否抓样)。
pub fn payload_has_redacted(payload: &serde_json::Value) -> bool {
    payload.get("redactedContent").is_some()
}

/// 抓一条样本。**永不 panic、永不返回错误** —— 诊断不能影响主链路。
///
/// - `client_model`:**客户端请求名**,不是映射后的 Kiro `modelId`(对抗评审 r2 #5 点名;
///   此处诚实命名,不假装是上游 id。要拿上游 id 得给 `async_stream_like` 加参,
///   不为一个诊断改热路径签名)。
/// - `seq`:本响应内第几个 redacted 帧(区分"单响应重复帧"与"多响应各一帧")。
/// - `thinking_block_open`:到达时是否已有 thinking 块开着 —— 这一位直接回答
///   「明文与 redacted 是否共存、谁先谁后」,是实现下行最需要的上下文。
///
/// 不记 account_id:学线缆形状不需要它,且它是敏感数据(codex 同意此取舍)。
pub fn capture(
    payload: &serde_json::Value,
    client_model: &str,
    seq: u32,
    thinking_block_open: bool,
) {
    let Some(line) = build_record_line(payload, client_model, seq, thinking_block_open) else {
        return;
    };
    let path = sample_path();
    // 【对抗评审 r2 #3】落盘挪到 `spawn_blocking`:次数有界 ≠ **单次延迟**有界
    // (磁盘拥塞/异常挂载会直接暂停 SSE 并占住 runtime worker)。
    // 原注释声称同步写换来"崩溃确定性"是错的 —— 没有 fsync,本就没有该保证,
    // 所以那个理由不成立,不该拿它换阻塞 runtime。
    let p = path.to_string();
    tokio::task::spawn_blocking(move || write_line(&p, &line));
}

/// 组装记录行;超限/触顶返回 None(不消耗额度)。与 IO 分离以便单测。
fn build_record_line(
    payload: &serde_json::Value,
    client_model: &str,
    seq: u32,
    thinking_block_open: bool,
) -> Option<String> {
    if SUCCEEDED.load(Ordering::Relaxed) >= MAX_SAMPLES {
        warn_capped_once("进程内条数已达上限");
        return None;
    }

    // 形状信息:字段名 + 各字段长度。**只对字符串字段取 len,非字符串只记类型名** ——
    // 原实现 `other.to_string().len()` 会把嵌套大对象整个序列化一遍(解码器允许 16MB 帧
    // → 多份 16MB 分配,对抗评审 r2 #2)。类型名对判形状同样够用。
    let obj = payload.as_object();
    let total_keys = obj.map(|o| o.len()).unwrap_or(0);
    let mut keys: Vec<&str> = obj
        .map(|o| o.keys().take(MAX_KEYS).map(|k| k.as_str()).collect())
        .unwrap_or_default();
    keys.sort_unstable();
    let field_lens: serde_json::Map<String, serde_json::Value> = obj
        .map(|o| {
            o.iter()
                .take(MAX_KEYS)
                .map(|(k, v)| {
                    let desc = match v {
                        serde_json::Value::String(s) => {
                            serde_json::json!(s.len().min(MAX_FIELD_LEN_REPORTED))
                        }
                        serde_json::Value::Null => serde_json::json!("null"),
                        serde_json::Value::Bool(_) => serde_json::json!("bool"),
                        serde_json::Value::Number(_) => serde_json::json!("number"),
                        serde_json::Value::Array(a) => serde_json::json!(format!("array[{}]", a.len())),
                        serde_json::Value::Object(m) => serde_json::json!(format!("object[{}]", m.len())),
                    };
                    (k.clone(), desc)
                })
                .collect()
        })
        .unwrap_or_default();

    let mut record = serde_json::json!({
        "ts_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        "seq_in_response": seq,
        "thinking_block_open": thinking_block_open,
        "client_model": client_model,
        "keys": keys,
        "keys_total": total_keys,
        "field_lens": field_lens,
        "payload_omitted": false,
        "payload": payload,
    });

    // 整条超限 → 丢掉 payload 只留形状(形状是主要目的,payload 是加分项)。
    let mut line = format!("{record}\n");
    if line.len() > MAX_RECORD_BYTES {
        record["payload"] = serde_json::Value::Null;
        record["payload_omitted"] = serde_json::json!(true);
        line = format!("{record}\n");
        // 形状本身还超限(键数极多等)→ 彻底放弃这条,不写半截。
        if line.len() > MAX_RECORD_BYTES {
            tracing::warn!(
                bytes = line.len(),
                "redactedContent 样本形状信息本身超出单条上限,已放弃该条(不写截断行)"
            );
            return None;
        }
    }
    Some(line)
}

/// 实际落盘。文件总字节到顶即停(跨重启有效的真上限)。成功后才记额度。
fn write_line(path: &str, line: &str) {
    // 文件已到总上限 → 不写。放在写之前,所以上限是硬的。
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() >= MAX_FILE_BYTES {
            warn_capped_once("样本文件已达总字节上限");
            return;
        }
    }
    // 0600:样本不保证只有密文,可能含明文 text —— gitignore 不是访问控制
    // (对抗评审 r2 #2)。用 mode() 在**创建时**就设权限,避免 create→chmod 的窗口。
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = opts
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    match res {
        Ok(()) => {
            // 【对抗评审 r2 #4】只有成功才消耗额度:否则前 20 次因目录/权限失败
            // 就会让进程此后永久失明。
            let n = SUCCEEDED.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                path = %path,
                seq = n,
                "抓到上游 reasoningContentEvent 携带 redactedContent —— 已落盘存证。\
                 我方尚未实现该支下行,本段推理被丢弃(其 signature 已显式忽略以免污染归属)"
            );
        }
        Err(e) => tracing::warn!(
            path = %path,
            error = %e,
            "redactedContent 样本落盘失败(不影响本次请求;额度未消耗,后续仍会重试)"
        ),
    }
}

fn warn_capped_once(why: &str) {
    if CAPPED_WARNED.swap(1, Ordering::Relaxed) == 0 {
        tracing::warn!(
            reason = %why,
            max_samples = MAX_SAMPLES,
            max_file_bytes = MAX_FILE_BYTES,
            "redactedContent 抓样已停止(样本足够看清形状;要继续抓请清空样本文件并重启)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_redacted_only_by_field_presence() {
        assert!(payload_has_redacted(&serde_json::json!({"redactedContent": "x"})));
        // 空串也算"携带":上游真发了这个字段就是形状证据,不该因为空而漏抓。
        assert!(payload_has_redacted(&serde_json::json!({"redactedContent": ""})));
        assert!(!payload_has_redacted(&serde_json::json!({"text": "t", "signature": "s"})));
        assert!(!payload_has_redacted(&serde_json::json!({})));
    }

    #[test]
    fn record_carries_shape_and_ordering_context() {
        let payload = serde_json::json!({
            "redactedContent": "Y2lwaGVy",
            "signature": "sig-abc",
            "text": "",
        });
        let line = build_record_line(&payload, "claude-opus-5", 3, true).expect("应产出记录");
        let rec: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(rec["keys"], serde_json::json!(["redactedContent", "signature", "text"]));
        assert_eq!(rec["field_lens"]["redactedContent"], 8);
        assert_eq!(rec["field_lens"]["text"], 0);
        // 实现下行最需要的两项上下文:共存/排序。
        assert_eq!(rec["seq_in_response"], 3);
        assert_eq!(rec["thinking_block_open"], true);
        // 诚实命名:这是客户端请求名,不是上游 modelId。
        assert_eq!(rec["client_model"], "claude-opus-5");
        assert_eq!(rec["payload_omitted"], false);
    }

    #[test]
    fn oversized_payload_is_dropped_but_shape_survives() {
        // 单条超限 → payload 丢掉、形状保留,且**不写截断的半截 JSON**。
        let big = "A".repeat(MAX_RECORD_BYTES * 2);
        let payload = serde_json::json!({"redactedContent": big, "signature": "s"});
        let line = build_record_line(&payload, "m", 0, false).expect("应仍产出形状记录");
        assert!(line.len() <= MAX_RECORD_BYTES, "实际 {}", line.len());
        let rec: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(rec["payload_omitted"], true);
        assert!(rec["payload"].is_null());
        // 形状信息必须完整保留 —— 它才是抓样的主要目的。
        assert_eq!(rec["field_lens"]["redactedContent"], MAX_RECORD_BYTES * 2);
        assert_eq!(rec["keys"], serde_json::json!(["redactedContent", "signature"]));
    }

    #[test]
    fn non_string_fields_recorded_by_type_not_serialized() {
        // 非字符串字段只记类型名 —— 不得把嵌套大对象整个序列化(16MB 帧 → 多份大分配)。
        let payload = serde_json::json!({
            "redactedContent": "x",
            "nested": {"a": 1, "b": 2},
            "arr": [1, 2, 3],
            "n": 5,
            "flag": true,
            "nil": null,
        });
        let line = build_record_line(&payload, "m", 0, false).unwrap();
        let rec: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(rec["field_lens"]["nested"], "object[2]");
        assert_eq!(rec["field_lens"]["arr"], "array[3]");
        assert_eq!(rec["field_lens"]["n"], "number");
        assert_eq!(rec["field_lens"]["flag"], "bool");
        assert_eq!(rec["field_lens"]["nil"], "null");
    }

    #[test]
    fn keys_are_capped_and_total_reported() {
        let mut m = serde_json::Map::new();
        m.insert("redactedContent".into(), serde_json::json!("x"));
        for i in 0..100 {
            m.insert(format!("k{i:03}"), serde_json::json!("v"));
        }
        let payload = serde_json::Value::Object(m);
        let line = build_record_line(&payload, "m", 0, false).unwrap();
        let rec: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(rec["keys"].as_array().unwrap().len(), MAX_KEYS, "keys 必须封顶");
        assert_eq!(rec["keys_total"], 101, "但真实键数要记下来");
    }

    #[test]
    fn file_byte_cap_is_hard_and_write_failure_costs_no_quota() {
        let dir = std::env::temp_dir().join(format!("caio-probe-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("s.jsonl");
        let p = path.to_string_lossy().to_string();

        // 预先把文件写到超过总上限 → write_line 必须拒写。
        std::fs::write(&path, "x".repeat((MAX_FILE_BYTES + 1) as usize)).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        write_line(&p, "{\"a\":1}\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            before,
            "文件到顶后不得再追加"
        );

        // 不可写路径:只 warn、不 panic,且**不消耗额度**。
        let q0 = SUCCEEDED.load(Ordering::Relaxed);
        write_line("/proc/definitely/not/writable.jsonl", "{\"a\":1}\n");
        assert_eq!(
            SUCCEEDED.load(Ordering::Relaxed),
            q0,
            "写失败不得消耗额度(否则前几次失败会让进程永久失明)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn sample_file_is_created_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("caio-probe-mode-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("m.jsonl");
        let p = path.to_string_lossy().to_string();
        write_line(&p, "{\"a\":1}\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "样本可能含明文,必须 0600 建文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
