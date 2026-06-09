//! Anthropic `usage` 对象构建 + 缓存命中上报 —— 🔵 逐字节搬自旧 `src/anthropic/usage.rs`。
//!
//! Anthropic API 的 usage 除 `input_tokens`/`output_tokens` 外,还可携带
//! `cache_read_input_tokens` 与 `cache_creation_input_tokens`。NewAPI 等中转网关按
//! 这些字段计费(cache_read 通常 0.1× 输入价)。
//!
//! ## 缓存上报模型(v53:统一走模拟器 + 三参数夹限)
//!
//! 历史上叠了"上游真值 / 模拟器 / metering 反推"三层,互相打架、产出错乱。**现行唯一
//! 路径**:完全由 prefix 缓存模拟器 [`crate::cache_sim`] 给出 `(hit_tokens, total_tokens)`,
//! 按下式算上报:
//!
//! ```text
//! frac     = hit / sim_total                        // 同口径(模拟器 tokenizer)比例
//! reported = clamp(frac × report_total × mult, report_total × floor, report_total × cap)
//! reported = clamp(reported, 0, report_total)       // 恒不超过 prompt 总量
//! uncached = report_total - reported                // cap<1 保证恒为正
//! ```
//!
//! 三个运营可调参数(admin 热调,见 config.cache):`multiplier`(默认 1.8)、
//! `cap_ratio`(默认 0.9,杜绝假到全命中)、`floor_ratio`(默认 0.0,冷启动如实报 0)。

/// 缩放倍率默认值。运行时实际值来自 config.cache.readMultiplier(admin 可热调)。
pub const DEFAULT_CACHE_READ_MULTIPLIER: f64 = 1.8;

/// 命中上限比率默认值(上报封顶 = total × 此值)。
pub const DEFAULT_CACHE_CAP_RATIO: f64 = 0.9;

/// 最低比率默认值(上报下限 = total × 此值)。0 = 冷启动如实报 0、不造假。
pub const DEFAULT_CACHE_FLOOR_RATIO: f64 = 0.0;

/// 按"模拟器命中比例 × 倍率、再用上下限比率夹住"算上报 cache_read。
///
/// **同口径修复**:`hit` 与 `sim_total` 都来自模拟器自己的 tokenizer(canon 串估算),
/// 二者比值 `frac = hit / sim_total` 才有稳定物理意义。而上报基准是 `report_total`
/// (Kiro tokenUsage/contextUsage 权威 token)。先算同口径比例,再映射到权威基准放大、夹限。
///
/// - `report_total <= 0`:返回 0(无上下文)。
/// - `sim_total <= 0`:命中比例无意义 → frac=0(仅由 floor 决定下限)。
///
/// 参数 clamp:`multiplier <= 0` 回退默认;`cap` 夹到 `[0,1]`;`floor` 夹到 `[0, cap]`。
pub fn reported_cache_read(
    report_total: i32,
    hit_tokens: i32,
    sim_total: i32,
    multiplier: f64,
    cap_ratio: f64,
    floor_ratio: f64,
) -> i32 {
    if report_total <= 0 {
        return 0;
    }
    let total = report_total as f64;
    // NaN/inf 防护(审查 Skeptic#7):config 热调值若为 NaN,f64::clamp 行为不可靠。
    // multiplier 非正(含 NaN,因 NaN>0.0 为 false)→ 回退默认;cap/floor 非有限 → 取默认。
    let mult = if multiplier > 0.0 {
        multiplier
    } else {
        DEFAULT_CACHE_READ_MULTIPLIER
    };
    let cap_ratio = if cap_ratio.is_finite() {
        cap_ratio
    } else {
        DEFAULT_CACHE_CAP_RATIO
    };
    let floor_ratio = if floor_ratio.is_finite() {
        floor_ratio
    } else {
        DEFAULT_CACHE_FLOOR_RATIO
    };
    let cap = cap_ratio.clamp(0.0, 1.0);
    let floor = floor_ratio.clamp(0.0, cap);

    let frac = if sim_total > 0 {
        (hit_tokens.max(0) as f64) / (sim_total as f64)
    } else {
        0.0
    };

    let scaled = frac * total * mult;
    let upper = total * cap;
    let lower = total * floor;
    let reported = scaled.clamp(lower, upper);
    (reported.round() as i32).clamp(0, report_total)
}

/// 构建 Anthropic `usage` JSON 对象。
///
/// **关键语义(对齐 Anthropic API 规范)**:`input_tokens` 只算未命中(新增)部分,
/// 不含缓存读取/创建;`cache_read_input_tokens` / `cache_creation_input_tokens` 单独列。
/// 总上下文 = input + cache_read + cache_creation。否则缓存部分被双重计费、用户多付。
///
/// 调用方传入"总上下文 input",本函数减去 cache_read / cache_creation 得 uncached_input。
pub fn build_usage_json(
    total_input_tokens: i32,
    output_tokens: i32,
    cache_read: i32,
    cache_creation: i32,
) -> serde_json::Value {
    let uncached_input = (total_input_tokens - cache_read - cache_creation).max(0);

    let mut obj = serde_json::Map::new();
    obj.insert("input_tokens".into(), serde_json::json!(uncached_input));
    obj.insert("output_tokens".into(), serde_json::json!(output_tokens));
    if cache_read > 0 {
        obj.insert("cache_read_input_tokens".into(), serde_json::json!(cache_read));
    }
    if cache_creation > 0 {
        obj.insert(
            "cache_creation_input_tokens".into(),
            serde_json::json!(cache_creation),
        );
    }
    serde_json::Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_total_returns_zero() {
        assert_eq!(reported_cache_read(0, 100, 1000, 1.8, 0.9, 0.0), 0);
        assert_eq!(reported_cache_read(-1, 100, 1000, 1.8, 0.9, 0.0), 0);
    }

    #[test]
    fn zero_sim_total_returns_floor() {
        assert_eq!(reported_cache_read(10000, 100, 0, 1.8, 0.9, 0.0), 0);
        assert_eq!(reported_cache_read(10000, 100, 0, 1.8, 0.9, 0.3), 3000);
    }

    #[test]
    fn zero_hit_with_floor_zero_reports_zero() {
        assert_eq!(reported_cache_read(10000, 0, 10000, 1.8, 0.9, 0.0), 0);
    }

    #[test]
    fn zero_hit_with_floor_lifts_to_floor() {
        assert_eq!(reported_cache_read(10000, 0, 10000, 1.8, 0.9, 0.3), 3000);
    }

    #[test]
    fn frac_scaled_below_cap() {
        assert_eq!(reported_cache_read(10000, 2000, 10000, 1.8, 0.9, 0.0), 3600);
    }

    #[test]
    fn frac_scaled_clamped_to_cap() {
        assert_eq!(reported_cache_read(10000, 6000, 10000, 1.8, 0.9, 0.0), 9000);
    }

    #[test]
    fn cross_tokenizer_uses_fraction_not_absolute() {
        assert_eq!(reported_cache_read(20000, 3000, 6000, 1.8, 0.9, 0.0), 18000);
        assert_ne!(reported_cache_read(20000, 3000, 6000, 1.8, 0.9, 0.0), 5400);
    }

    #[test]
    fn reported_never_exceeds_total() {
        assert_eq!(reported_cache_read(1000, 800, 1000, 1.8, 1.0, 0.0), 1000);
    }

    #[test]
    fn preserves_variation_shape() {
        let total = 11000;
        let r_low = reported_cache_read(total, 1500, total, 1.8, 0.9, 0.0);
        let r_high = reported_cache_read(total, 5000, total, 1.8, 0.9, 0.0);
        assert!(r_low < r_high, "上报值应随命中升高: {r_low} vs {r_high}");
        assert_eq!(r_low, 2700);
        assert_eq!(r_high, 9000);
    }

    #[test]
    fn multiplier_nonpositive_falls_back_to_default() {
        assert_eq!(
            reported_cache_read(10000, 2000, 10000, 0.0, 0.9, 0.0),
            reported_cache_read(10000, 2000, 10000, DEFAULT_CACHE_READ_MULTIPLIER, 0.9, 0.0)
        );
    }

    #[test]
    fn floor_clamped_below_cap() {
        assert_eq!(reported_cache_read(10000, 0, 10000, 1.8, 0.5, 0.9), 5000);
    }

    #[test]
    fn nan_config_falls_back_to_defaults_no_panic() {
        // 审查 Skeptic#7:NaN 配置不应 panic,回退默认参数。
        let r = reported_cache_read(10000, 2000, 10000, f64::NAN, f64::NAN, f64::NAN);
        // 等价于默认 (1.8, 0.9, 0.0):frac=0.2 ×10000×1.8 = 3600 < cap 9000
        assert_eq!(r, reported_cache_read(10000, 2000, 10000, 1.8, 0.9, 0.0));
        // inf 同样安全
        let _ = reported_cache_read(10000, 2000, 10000, f64::INFINITY, f64::INFINITY, f64::INFINITY);
    }

    #[test]
    fn build_usage_omits_zero_cache_fields_and_keeps_full_input() {
        let v = build_usage_json(1000, 50, 0, 0);
        assert_eq!(v["input_tokens"], 1000);
        assert_eq!(v["output_tokens"], 50);
        assert!(v.get("cache_read_input_tokens").is_none());
        assert!(v.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn build_usage_subtracts_cache_from_input() {
        let v = build_usage_json(1000, 50, 800, 50);
        assert_eq!(v["input_tokens"], 150);
        assert_eq!(v["cache_read_input_tokens"], 800);
        assert_eq!(v["cache_creation_input_tokens"], 50);
        let sum = v["input_tokens"].as_i64().unwrap()
            + v["cache_read_input_tokens"].as_i64().unwrap()
            + v["cache_creation_input_tokens"].as_i64().unwrap();
        assert_eq!(sum, 1000);
    }

    #[test]
    fn build_usage_clamps_negative_uncached_to_zero() {
        let v = build_usage_json(100, 5, 200, 0);
        assert_eq!(v["input_tokens"], 0);
        assert_eq!(v["cache_read_input_tokens"], 200);
    }

    #[test]
    fn build_usage_inflated_cache_scenario() {
        let v = build_usage_json(6000, 100, 5100, 0);
        assert_eq!(v["input_tokens"], 900);
        assert_eq!(v["cache_read_input_tokens"], 5100);
    }
}
