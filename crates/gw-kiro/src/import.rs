//! KiroManager 导出 JSON → 账号字段映射(完整导入)。
//!
//! **为什么存在**:KiroManager 导出的每个账号顶层带真机 `machineId`(激活时绑定的
//! 设备指纹)。只导 refreshToken 会丢掉它,服务器据 rt 重新派生一个不同的 machineId
//! → 上游看到"激活设备 A、发包设备 B" = 双指纹 = 封号。完整导入把 machineId 及
//! clientId/secret/profileArn 等**原样**搬进账号 extra,消除这一根因。
//!
//! 本模块只做**纯映射**(KiroManager JSON → `extra` map),不碰库;智能合并(已存在
//! 账号只补缺失字段、不覆盖服务器已 roll 的 token)由 gw-app admin 处理。

use serde_json::{json, Map, Value};

/// 一个映射好的待导入账号(account_id + 完整 extra)。
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedAccount {
    pub account_id: String,
    pub extra: Map<String, Value>,
}

impl ImportedAccount {
    /// extra 里是否带显式 machineId(供 admin 报告/对账)。
    pub fn has_machine_id(&self) -> bool {
        self.extra
            .get("machine_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    }
}

/// 解析 KiroManager 导出根对象(`{ version, accounts: [...] }`)。
/// 跳过无 refreshToken 的条目(social/IdC 都需要它)。全部无效返回 `Err`。
pub fn parse_kiromanager_export(root: &Value) -> Result<Vec<ImportedAccount>, String> {
    let accounts = root
        .get("accounts")
        .and_then(|a| a.as_array())
        .ok_or("JSON 缺少 accounts 数组(不是 KiroManager 导出格式?)")?;

    let mut out = Vec::new();
    let mut skipped_no_rt = 0;
    for acc in accounts {
        match map_one(acc) {
            Some(m) => out.push(m),
            None => skipped_no_rt += 1,
        }
    }
    if out.is_empty() {
        return Err(format!(
            "未解析到任何有效账号({} 条缺 refreshToken 被跳过)",
            skipped_no_rt
        ));
    }
    Ok(out)
}

fn map_one(acc: &Value) -> Option<ImportedAccount> {
    let creds = acc.get("credentials");
    // refresh_token 必需,缺失则跳过(无法刷新)。
    let refresh = str_at(creds, "refreshToken").filter(|s| !s.is_empty())?;

    let mut extra = Map::new();

    // 1. machineId(封号关键)—— 顶层。**必须校验/归一**:运行时只认 64hex/UUID
    //    (machine_id::normalize),否则会静默回退到按 rt 派生 = 重新引入封号
    //    (审查 Skeptic#2/Architect#3)。非法形态不写入,留空让冻结按 rt 派生并显式提示。
    if let Some(mid) = acc.get("machineId").and_then(|v| v.as_str()) {
        if let Some(normalized) = crate::machine_id::normalize_machine_id(mid) {
            extra.insert("machine_id".into(), json!(normalized));
        }
    }

    // 2. 凭据。
    extra.insert("refresh_token".into(), json!(refresh));
    if let Some(at) = str_at(creds, "accessToken").filter(|s| !s.is_empty()) {
        extra.insert("access_token".into(), json!(at));
    }
    // expiresAt(epoch ms)→ expires_at RFC3339(供 worker has_fresh_token 判断)。
    if let Some(ms) = num_at(creds, "expiresAt") {
        extra.insert(
            "expires_at".into(),
            json!(crate::token::format_unix_utc(ms / 1000)),
        );
    }
    if let Some(cid) = str_at(creds, "clientId").filter(|s| !s.is_empty()) {
        extra.insert("client_id".into(), json!(cid));
    }
    if let Some(cs) = str_at(creds, "clientSecret").filter(|s| !s.is_empty()) {
        extra.insert("client_secret".into(), json!(cs));
    }
    if let Some(region) = str_at(creds, "region").filter(|s| !s.is_empty()) {
        extra.insert("region".into(), json!(region));
    }
    if let Some(arn) = str_at(creds, "profileArn").filter(|s| !s.is_empty()) {
        extra.insert("profile_arn".into(), json!(arn));
    }

    // 3. 身份来源 kiro_provider:供 profileArn 兜底 + internal 头判定。
    //    取 credentials.provider,回退顶层 idp。**不**映射 authMethod(IdC 分流看
    //    client_id+secret;authMethod=external_idp 会误触发 TokenType 头)。
    let idp = str_at(creds, "provider")
        .or_else(|| acc.get("idp").and_then(|v| v.as_str()))
        .map(normalize_kiro_provider);
    if let Some(p) = idp.filter(|s| !s.is_empty()) {
        extra.insert("kiro_provider".into(), json!(p));
    }

    // 4. 展示/对账字段。userId 是上游唯一稳定身份,存下来供智能合并防碰撞校验
    //    (邮箱清洗派生的 account_id 可能碰撞,合并前用它确认是不是同一个真号)。
    let email = acc.get("email").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    if let Some(e) = email {
        extra.insert("email".into(), json!(e));
    }
    if let Some(uid) = acc.get("userId").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        extra.insert("user_id".into(), json!(uid));
    }
    if let Some(nick) = acc.get("nickname").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        extra.insert("nickname".into(), json!(nick));
    }
    if let Some(title) = acc
        .get("subscription")
        .and_then(|s| s.get("title").or_else(|| s.get("type")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        extra.insert("subscription_title".into(), json!(title));
    }

    // account_id:优先 email(可读、唯一),回退 userId / id。清洗成合法路径段。
    let raw_id = email
        .or_else(|| acc.get("userId").and_then(|v| v.as_str()))
        .or_else(|| acc.get("id").and_then(|v| v.as_str()))
        .unwrap_or("kiro-account");
    let account_id = sanitize_account_id(raw_id);

    Some(ImportedAccount { account_id, extra })
}

/// KiroManager 的 provider/idp 值 → 本项目 kiro_provider 取值(小写归一)。
/// 已知:Enterprise / BuilderId / GitHub / Google / Social;其余原样小写。
fn normalize_kiro_provider(v: &str) -> String {
    match v.trim().to_ascii_lowercase().as_str() {
        "builderid" | "builder-id" | "builder_id" => "builderid".to_string(),
        "github" => "github".to_string(),
        "google" => "google".to_string(),
        "enterprise" => "enterprise".to_string(),
        other => other.to_string(),
    }
}

/// 清洗成合法 account_id(对齐 admin validate_account_id:字母数字 + `- _ . ~`,1–64)。
/// 非法字符替换为 `-`;空 → "kiro-account";超 64 截断。
fn sanitize_account_id(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.len() > 64 {
        s.truncate(64);
        // 截断可能落在多字节边界外(此处全 ASCII,安全),并去尾部多余 '-'。
        while s.ends_with('-') && s.len() > 1 {
            s.pop();
        }
    }
    if s.is_empty() {
        return "kiro-account".to_string();
    }
    s
}

/// 取 `obj.field` 的字符串值(obj 为 Option<&Value>)。
fn str_at<'a>(obj: Option<&'a Value>, field: &str) -> Option<&'a str> {
    obj.and_then(|o| o.get(field)).and_then(|v| v.as_str())
}

/// 取 `obj.field` 的整数值(兼容 JSON 整数/浮点/数字字符串——某些导出把 epoch 写成字符串)。
fn num_at(obj: Option<&Value>, field: &str) -> Option<i64> {
    obj.and_then(|o| o.get(field)).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enterprise_export() -> Value {
        json!({
            "version": "1.7.5",
            "accounts": [{
                "email": "mrdev+mrdev2947@example.com",
                "userId": "d-9066xxxx",
                "nickname": "mrdev+mrdev2947",
                "idp": "Enterprise",
                "machineId": "3e3aa9".to_string() + &"0".repeat(58),
                "credentials": {
                    "accessToken": "aoaAAA_access",
                    "refreshToken": "aorAAA_refresh",
                    "clientId": "Gv-mRC_client",
                    "clientSecret": "eyJraW_secret",
                    "region": "us-east-1",
                    "startUrl": "https://example.awsapps.com/start",
                    "expiresAt": 1781121312584i64,
                    "authMethod": "IdC",
                    "provider": "Enterprise",
                    "profileArn": "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK"
                },
                "subscription": {"type": "Enterprise", "title": "KIRO POWER"}
            }]
        })
    }

    #[test]
    fn maps_all_critical_fields() {
        let out = parse_kiromanager_export(&enterprise_export()).unwrap();
        assert_eq!(out.len(), 1);
        let a = &out[0];
        // account_id 由 email 清洗(@ 和 + 变 -)。
        assert_eq!(a.account_id, "mrdev-mrdev2947-example.com");
        // machineId 必须搬进来(封号关键)。
        assert!(a.has_machine_id());
        assert_eq!(a.extra["machine_id"], json!("3e3aa9".to_string() + &"0".repeat(58)));
        assert_eq!(a.extra["refresh_token"], json!("aorAAA_refresh"));
        assert_eq!(a.extra["access_token"], json!("aoaAAA_access"));
        assert_eq!(a.extra["client_id"], json!("Gv-mRC_client"));
        assert_eq!(a.extra["client_secret"], json!("eyJraW_secret"));
        assert_eq!(a.extra["region"], json!("us-east-1"));
        assert_eq!(a.extra["profile_arn"].as_str().unwrap(), "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK");
        assert_eq!(a.extra["kiro_provider"], json!("enterprise"));
        assert_eq!(a.extra["email"], json!("mrdev+mrdev2947@example.com"));
        assert_eq!(a.extra["subscription_title"], json!("KIRO POWER"));
        // authMethod 不得进 auth_method(否则误触发 TokenType 头)。
        assert!(!a.extra.contains_key("auth_method"), "authMethod 不应映射");
        // expiresAt(ms)→ RFC3339。
        let exp = a.extra["expires_at"].as_str().unwrap();
        assert!(exp.ends_with('Z') && exp.contains('T'), "expires_at 应为 RFC3339: {exp}");
    }

    #[test]
    fn builderid_maps_provider_for_arn_fallback() {
        let v = json!({"accounts": [{
            "email": "david.hunter@example.com",
            "idp": "BuilderId",
            "machineId": "756d0d".to_string() + &"0".repeat(58),
            "credentials": {
                "refreshToken": "rt", "clientId": "c", "clientSecret": "s",
                "region": "us-east-1", "provider": "BuilderId"
            }
        }]});
        let out = parse_kiromanager_export(&v).unwrap();
        assert_eq!(out[0].extra["kiro_provider"], json!("builderid"));
        // BuilderId 无 profileArn —— 不臆造,留空靠 kiro_provider 兜底 BUILDER_ID ARN。
        assert!(!out[0].extra.contains_key("profile_arn"));
    }

    #[test]
    fn skips_accounts_without_refresh_token() {
        let v = json!({"accounts": [
            {"email": "a@x.com", "credentials": {"accessToken": "at"}},
            {"email": "b@x.com", "credentials": {"refreshToken": "rt"}}
        ]});
        let out = parse_kiromanager_export(&v).unwrap();
        assert_eq!(out.len(), 1, "无 refreshToken 的条目应跳过");
        assert_eq!(out[0].account_id, "b-x.com");
    }

    #[test]
    fn invalid_machine_id_is_rejected_not_stored() {
        // 非法 machineId(非 64hex/UUID)不得写入——否则 has_machine_id 谎报 true,
        // 运行时仍按 rt 派生 = 重新引入封号(审查 Skeptic#2)。
        let v = json!({"accounts": [{
            "email": "x@y.com",
            "machineId": "abc",
            "credentials": {"refreshToken": "rt"}
        }]});
        let out = parse_kiromanager_export(&v).unwrap();
        assert!(!out[0].has_machine_id(), "非法 machineId 不应被当成已设置");
        assert!(!out[0].extra.contains_key("machine_id"));
    }

    #[test]
    fn string_expires_at_is_parsed() {
        let v = json!({"accounts": [{
            "email": "x@y.com",
            "credentials": {"refreshToken": "rt", "accessToken": "at", "expiresAt": "1781121312584"}
        }]});
        let out = parse_kiromanager_export(&v).unwrap();
        let exp = out[0].extra["expires_at"].as_str().unwrap();
        assert!(exp.ends_with('Z') && exp.contains('T'), "字符串 epoch 也应转 RFC3339: {exp}");
    }

    #[test]
    fn errors_when_no_accounts_array() {
        assert!(parse_kiromanager_export(&json!({"version": "1.7.5"})).is_err());
        assert!(parse_kiromanager_export(&json!({"accounts": []})).is_err());
    }

    #[test]
    fn sanitize_account_id_rules() {
        assert_eq!(sanitize_account_id("a@b+c.com"), "a-b-c.com");
        assert_eq!(sanitize_account_id(""), "kiro-account");
        assert_eq!(sanitize_account_id(&"x".repeat(100)).len(), 64);
    }
}
