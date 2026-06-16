//! 工具调用参数防御性修复。
//!
//! 部分上游模型(实测 Kiro 上的 Opus 会把 `AskUserQuestion.questions` 双重编码——
//! 2026-06-16 线上取证:1027 次 AskUserQuestion 调用中 223 次把本应是 JSON array 的
//! `questions` 序列化成了 JSON **字符串**)会把本应是 array/object 的工具参数
//! 序列化成 JSON 字符串。客户端按工具 input_schema 校验时拒收:
//! `The parameter questions type is expected as array but provided as string`。
//!
//! 反代只把模型产出的 tool input 逐字透传,本身无 bug;但既然这是唯一可控点,
//! 就在收尾组装 tool input 时按 schema 解包修复。
//!
//! 安全边界(绝不破坏正常输入):
//! - 只对该工具 **input_schema 声明为 array/object 的顶层字段**生效;
//! - 只当该字段当前值是**字符串**、且能解析成对应类型时才替换;
//! - 解析失败 / 类型不符 / 顶层非 object / 输入非完整 JSON → 一律原样保留。

use std::collections::HashMap;
use std::collections::HashSet;

use serde_json::Value;

/// 从工具的 `input_schema`(Anthropic 风格 JSON Schema)提取顶层 `type` 为 `array`/`object`
/// 的字段名集合。无 `properties` 或无匹配字段 → 空集(该工具不参与修复)。
pub fn array_object_fields(input_schema: &HashMap<String, Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(props) = input_schema.get("properties").and_then(Value::as_object) else {
        return out;
    };
    for (name, spec) in props {
        if let Some(t) = spec.get("type").and_then(Value::as_str) {
            if t == "array" || t == "object" {
                out.insert(name.clone());
            }
        }
    }
    out
}

/// 就地修复一个**已解析**的工具 input 对象:把被双重编码成字符串的 array/object 字段解包。
/// 返回是否发生过修改。
pub fn repair_value(value: &mut Value, fields: &HashSet<String>) -> bool {
    if fields.is_empty() {
        return false;
    }
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for field in fields {
        let s = match obj.get(field) {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        if let Ok(parsed) = serde_json::from_str::<Value>(&s) {
            if parsed.is_array() || parsed.is_object() {
                obj.insert(field.clone(), parsed);
                changed = true;
            }
        }
    }
    changed
}

/// 字符串入口(流式 `partial_json` / 缓冲用):解析 → 修复 → 重新序列化。
/// 未修改返回**原串**;非完整/非 object JSON → 原样返回,绝不破坏。
pub fn repair_str(input: &str, fields: &HashSet<String>) -> String {
    if fields.is_empty() || input.is_empty() {
        return input.to_string();
    }
    let mut value: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return input.to_string(),
    };
    if repair_value(&mut value, fields) {
        serde_json::to_string(&value).unwrap_or_else(|_| input.to_string())
    } else {
        input.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extract_array_object_fields_from_schema() {
        let schema: HashMap<String, Value> = serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "questions": { "type": "array" },
                "config": { "type": "object" },
                "header": { "type": "string" }
            }
        }))
        .unwrap();
        let f = array_object_fields(&schema);
        assert!(f.contains("questions"));
        assert!(f.contains("config"));
        assert!(!f.contains("header"));
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn no_properties_yields_empty() {
        let schema: HashMap<String, Value> =
            serde_json::from_value(json!({ "type": "object" })).unwrap();
        assert!(array_object_fields(&schema).is_empty());
    }

    #[test]
    fn unwraps_double_encoded_array() {
        let input = r#"{"questions":"[{\"header\":\"H\"}]"}"#;
        let out = repair_str(input, &fields(&["questions"]));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["questions"].is_array(), "questions 应被解包成数组: {out}");
        assert_eq!(v["questions"][0]["header"], "H");
    }

    #[test]
    fn unwraps_double_encoded_object() {
        let input = r#"{"config":"{\"a\":1}"}"#;
        let out = repair_str(input, &fields(&["config"]));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["config"].is_object());
        assert_eq!(v["config"]["a"], 1);
    }

    #[test]
    fn leaves_correct_array_untouched() {
        let input = r#"{"questions":[{"header":"H"}]}"#;
        let out = repair_str(input, &fields(&["questions"]));
        assert_eq!(out, input);
    }

    #[test]
    fn leaves_legit_string_field_untouched() {
        let input = r#"{"header":"[not really]"}"#;
        let out = repair_str(input, &fields(&["questions"]));
        assert_eq!(out, input);
    }

    #[test]
    fn does_not_unwrap_scalar_string() {
        let input = r#"{"questions":"5"}"#;
        let out = repair_str(input, &fields(&["questions"]));
        assert_eq!(out, input);
    }

    #[test]
    fn does_not_touch_non_json_string() {
        let input = r#"{"questions":"hello world"}"#;
        let out = repair_str(input, &fields(&["questions"]));
        assert_eq!(out, input);
    }

    #[test]
    fn incomplete_json_returned_verbatim() {
        let input = r#"{"questions":"[{"#;
        let out = repair_str(input, &fields(&["questions"]));
        assert_eq!(out, input);
    }

    #[test]
    fn empty_fields_is_noop() {
        let input = r#"{"questions":"[1,2]"}"#;
        let out = repair_str(input, &HashSet::new());
        assert_eq!(out, input);
    }

    #[test]
    fn repairs_one_field_keeps_siblings() {
        let input = r#"{"header":"choose","questions":"[{\"a\":1}]"}"#;
        let out = repair_str(input, &fields(&["questions"]));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["header"], "choose");
        assert!(v["questions"].is_array());
    }
}
