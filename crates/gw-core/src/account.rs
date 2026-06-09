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
    /// provider 专属字段(原样保留,provider 负责反序列化为自身类型)。
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_max_concurrency() -> u32 {
    1
}

impl Account {
    /// 读取 extra 中的字符串字段。
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(|v| v.as_str())
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
        assert_eq!(acct.max_concurrency, 1); // default
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
}
