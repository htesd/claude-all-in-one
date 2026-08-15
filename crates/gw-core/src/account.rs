//! 账号与字段 schema。
//!
//! [`FieldSpec`] 由 [`crate::provider::Provider::account_schema`] 返回,
//! 同时驱动:① admin 前端动态渲染表单;② accounts.yaml 字段校验。
//! 借鉴 ALLinOne `base.py::account_schema`,但用强类型 struct 替代裸 dict。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 账号字段类型(决定前端控件 + 校验)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Password,
    Int,
    Bool,
}

/// 单个账号字段定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSpec {
    /// 字段名(也是写入 accounts.yaml 的 key)。
    pub name: &'static str,
    /// 显示名。
    pub label: &'static str,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    pub required: bool,
    /// 可选提示文本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<&'static str>,
}

impl FieldSpec {
    pub const fn new(
        name: &'static str,
        label: &'static str,
        field_type: FieldType,
        required: bool,
    ) -> Self {
        Self {
            name,
            label,
            field_type,
            required,
            help: None,
        }
    }

    /// 链式附加提示文本(const,可用于 static schema 表)。
    pub const fn with_help(mut self, help: &'static str) -> Self {
        self.help = Some(help);
        self
    }
}

/// 一个账号实例。
///
/// 公共字段是所有 provider 共有的;provider 专属字段(refresh_token /
/// proxy / region 等)放进 `extra`,由各 provider 自己解读
/// (对应 ALLinOne 的 `AccountConfig.extra`,但保留为 JSON 而非 Any)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// 唯一标识。
    pub account_id: String,
    /// 所属 provider 家族(如 "kiro")。
    ///
    /// 在 accounts.yaml 的分组格式里,provider 写在组级别(`AccountGroup.provider`),
    /// 账号级省略,由加载层从组传播下来,故此处 `serde(default)`。
    #[serde(default)]
    pub provider: String,
    /// 最大并发(本账号同时在途请求数上限)。
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    /// 是否禁用。
    #[serde(default)]
    pub disabled: bool,
    /// 建档时刻(Unix 秒,来自 accounts.created_at;对 restock 补货号≈上游激活时间)。
    /// 低优先新号暖机按它算号龄。**0 = 未知**(accounts.yaml 降级加载/手工构造):
    /// 未知号龄按「已毕业」处理、不暖机 —— fail-open 到引入暖机前的既有行为,
    /// 绝不因元数据缺失把老号误判成新号限流。
    #[serde(default)]
    pub created_at: i64,
    /// provider 专属字段(原样保留,provider 负责反序列化为自身类型)。
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_max_concurrency() -> u32 {
    // 对齐 kiro.rs 生产默认(credential.maxConcurrency=2):单号串行会让同会话的
    // 并行 tool-call / 子代理请求排队。DB 中已有显式值的存量账号不受影响。
    2
}

impl Account {
    /// 读取 extra 中的字符串字段。
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(|v| v.as_str())
    }
}

/// 账号级模型白名单的 extra 键名。它是**通用路由策略**(不是某个 provider 私有),
/// 规范存储为 JSON 字符串数组;写侧(admin PATCH)负责校验与归一。
pub const MODEL_ALLOWLIST_KEY: &str = "model_allowlist";

/// 账号是否允许服务 `model`。`model` 应是**上游侧**模型名 —— 调用方先过自己
/// 真正发包用的那个映射函数再来问(cursor 是 `resolve_cursor_model`),否则
/// 白名单写的上游名对不上客户端发来的别名/带日期后缀名。
///
/// 语义(gpt-5.6-sol 评审定稿,fail-open **只给「未配置」**):
/// - 键缺失或为 `null` → 不限。写侧用 `null` 表达「清除白名单」
///   (`merge_account_extra` 是读-合-写,写 null 不删键,两者必须同义);
///   配置缺失绝不能把健康号从池里摘掉。
/// - 合法非空列表 → 按列表限制。条目大小写不敏感;`前缀*`(星号仅限末尾,
///   写侧校验)做前缀匹配,其余全等 —— `grok-4.5` 不顺带放行 `grok-4.6`。
/// - 空数组 / 空串 / 类型错 → **全禁**(fail-closed)。写侧拒绝这些形态,
///   能出现只可能是手改 DB/YAML —— 运维写空值的本意多半是「全禁」,
///   解释成「全放」是要出事的。
/// - 兼容:值为字符串时按逗号/分号/空白分隔解析(手写 accounts.yaml 的自然写法)。
///
/// 性能契约:调度器在锁内对每个候选账号调用(kiro 组 370 号规模),
/// 本函数只读 `extra`、零分配(大小写比较用 `eq_ignore_ascii_case`)。
pub fn model_allowlist_allows(account: &Account, model: &str) -> bool {
    let Some(raw) = account.extra.get(MODEL_ALLOWLIST_KEY) else {
        return true;
    };
    match raw {
        serde_json::Value::Null => true,
        // 空数组 / 全垃圾条目(非字符串、空串)→ any = false = 全禁,不是全放。
        serde_json::Value::Array(items) => items.iter().any(|it| {
            it.as_str()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .is_some_and(|p| allowlist_pattern_matches(p, model))
        }),
        serde_json::Value::String(s) => s
            .split([',', ';'])
            .flat_map(str::split_whitespace)
            .any(|p| allowlist_pattern_matches(p, model)),
        // 数字 / 布尔 / 对象:类型错 —— fail-closed(见上)。
        _ => false,
    }
}

/// `前缀*`(星号仅限末尾)做大小写不敏感前缀匹配,其余全等。
/// 星号在中间的非法条目(只能出自手改)按字面比较 —— 模型名里没有 `*`,
/// 等效于该条目匹配不到任何东西,不放大也不缩小其它条目的语义。
fn allowlist_pattern_matches(pat: &str, model: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => {
            model.len() >= prefix.len()
                && model.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        }
        None => pat.eq_ignore_ascii_case(model),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_spec_serializes_type_key() {
        let f = FieldSpec::new("account_id", "账号ID", FieldType::String, true);
        let j = serde_json::to_value(&f).unwrap();
        assert_eq!(j["type"], "string");
        assert_eq!(j["name"], "account_id");
        // help 为 None 时不出现
        assert!(j.get("help").is_none());
    }

    #[test]
    fn account_extra_captures_unknown_fields() {
        let yaml = r#"
account_id: k1
provider: kiro
refresh_token: tok123
region: us-east-1
"#;
        let acct: Account = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(acct.account_id, "k1");
        assert_eq!(acct.max_concurrency, 2); // default(对齐 kiro.rs)
        assert_eq!(acct.extra_str("refresh_token"), Some("tok123"));
        assert_eq!(acct.extra_str("region"), Some("us-east-1"));
    }

    #[test]
    fn account_explicit_fields_not_in_extra() {
        let yaml = r#"
account_id: k1
provider: kiro
max_concurrency: 4
disabled: true
"#;
        let acct: Account = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(acct.max_concurrency, 4);
        assert!(acct.disabled);
        assert!(acct.extra.is_empty());
    }

    fn allowlist_acct(v: Option<serde_json::Value>) -> Account {
        let mut extra = BTreeMap::new();
        if let Some(v) = v {
            extra.insert(MODEL_ALLOWLIST_KEY.to_string(), v);
        }
        Account {
            account_id: "a".into(),
            provider: "cursor".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra,
        }
    }

    #[test]
    fn model_allowlist_缺失与_null_等于不限() {
        // fail-open 只给「未配置」;写侧用 null 表达清除(merge_account_extra
        // 是读-合-写,写 null 不删键,两者必须同义)。
        assert!(model_allowlist_allows(&allowlist_acct(None), "claude-opus-5"));
        assert!(model_allowlist_allows(
            &allowlist_acct(Some(serde_json::Value::Null)),
            "claude-opus-5"
        ));
    }

    #[test]
    fn model_allowlist_列表限制与末尾通配() {
        let a = allowlist_acct(Some(serde_json::json!(["default", "composer*", "grok*"])));
        assert!(model_allowlist_allows(&a, "default"));
        assert!(model_allowlist_allows(&a, "composer-2.5"));
        assert!(model_allowlist_allows(&a, "grok-4.6"), "末尾通配放行未来型号");
        assert!(!model_allowlist_allows(&a, "claude-opus-5"));
        // 大小写不敏感;全等不吃前缀(grok-4.5 不顺带放行 grok-4.6)。
        assert!(model_allowlist_allows(
            &allowlist_acct(Some(serde_json::json!(["GROK-4.5"]))),
            "grok-4.5"
        ));
        assert!(!model_allowlist_allows(
            &allowlist_acct(Some(serde_json::json!(["grok-4.5"]))),
            "grok-4.6"
        ));
        // CSV 字符串写法(手写 accounts.yaml 的自然形态)也收。
        let s = allowlist_acct(Some(serde_json::json!("default, composer* ;grok*")));
        assert!(model_allowlist_allows(&s, "grok-4.5"));
        assert!(!model_allowlist_allows(&s, "gpt-5.6-sol"));
    }

    #[test]
    fn model_allowlist_空表与类型错全禁() {
        // 写侧拒绝这些形态,能出现只可能是手改 DB/YAML —— 按「全禁」而不是
        // 「全放」:运维写空值的本意多半是全禁(gpt-5.6-sol 评审定稿)。
        assert!(!model_allowlist_allows(&allowlist_acct(Some(serde_json::json!([]))), "default"));
        assert!(!model_allowlist_allows(&allowlist_acct(Some(serde_json::json!(""))), "default"));
        assert!(!model_allowlist_allows(&allowlist_acct(Some(serde_json::json!(123))), "default"));
        assert!(!model_allowlist_allows(
            &allowlist_acct(Some(serde_json::json!({"a": 1}))),
            "default"
        ));
        // 星号在中间(非法,写侧拒绝)按字面比较 = 该条目匹配不到任何模型,
        // 不放大也不缩小其它条目的语义。
        assert!(!model_allowlist_allows(
            &allowlist_acct(Some(serde_json::json!(["gr*k"]))),
            "grok-4.5"
        ));
    }
}
