//! 账号导出 JSON → 账号字段映射(完整导入)。支持**两种**输入格式:
//!
//! 1. **KiroManager 导出**:`{ version, accounts: [{ email, machineId,
//!    credentials: { refreshToken, accessToken, clientId, ... }, subscription }] }`
//!    (嵌套 + camelCase)。每账号顶层带真机 `machineId`(激活时的设备指纹),
//!    只导 refreshToken 会丢它 → 服务器据 rt 派生不同 machineId → 双指纹封号;
//!    完整导入把 machineId/clientId/secret/profileArn **原样**搬进 extra 消除根因。
//! 2. **扁平账号数组**:`[{ refresh_token, access_token, client_id, client_secret,
//!    profile_arn, region, auth_method, email, expires_at }, ...]`(顶层 + snake_case)。
//!    字段名已是内部 extra 约定,近乎恒等映射;这类导出通常**不带** machineId
//!    (留空 → 按 rt 派生,封号风险见上,建议尽量用带 machineId 的来源)。
//!
//! 顶层是**数组** → 按扁平解析;是**对象且含 `accounts`** → 逐账号按"有无 `credentials`
//! 子对象"分流到嵌套/扁平映射。本模块只做**纯映射**(JSON → `extra` map),不碰库;
//! 智能合并(已存在账号只补缺失字段、不覆盖服务器已 roll 的 token)由 gw-app admin 处理。

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

/// 解析账号导出(顶层数组 = 扁平;`{ accounts: [...] }` = KiroManager/混合)。
/// 跳过无 refreshToken 的条目(social/IdC 都需要它)。全部无效返回 `Err`。
pub fn parse_accounts_export(root: &Value) -> Result<Vec<ImportedAccount>, String> {
    let accounts: &[Value] = match root.as_array() {
        Some(arr) => arr.as_slice(),
        None => root
            .get("accounts")
            .and_then(|a| a.as_array())
            .map(Vec::as_slice)
            .ok_or("JSON 既不是账号数组,也不含 accounts 数组(不是支持的导出格式?)")?,
    };

    let mut out = Vec::new();
    let mut skipped_no_rt = 0;
    for acc in accounts {
        match map_account(acc) {
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

/// 单账号分流:含 `credentials` 子对象 → KiroManager 嵌套([`map_one`]);
/// 否则 → 扁平 snake_case([`map_flat`])。
fn map_account(acc: &Value) -> Option<ImportedAccount> {
    if acc.get("credentials").is_some() {
        map_one(acc)
    } else {
        map_flat(acc)
    }
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

/// 扁平 snake_case 账号(字段即 extra,顶层无 `credentials`)→ [`ImportedAccount`]。
/// refresh_token 必需。字段名已是内部约定,近乎恒等映射。machineId 不在则留空(派生)。
///
/// 与嵌套路径的差别:这里 `auth_method` 是我方 snake_case 约定值(`idc`/`social`/
/// `external_idp`),**按原值搬运**(权威);嵌套路径来自 KiroManager 的 `authMethod="IdC"`
/// 自有约定,故刻意不映射。`external_idp` 才会触发 TokenType 头,`idc` 安全。
fn map_flat(acc: &Value) -> Option<ImportedAccount> {
    // refresh_token 必需。flat_str 已 snake→camel 回退,故 refreshToken 也认。
    let refresh = flat_str(acc, "refresh_token").filter(|s| !s.is_empty())?;

    let mut extra = Map::new();
    extra.insert("refresh_token".into(), json!(refresh));

    // 凭据/配置字段(非空才写)。flat_str: snake_case 优先,回退 camelCase——
    // 兼容 kiro.rs 风格的 camelCase 导出(clientId/clientSecret/accessToken/profileArn/
    // authMethod),否则这些字段静默丢失 → BuilderId 号丢 client 凭据、刷新分流判错。
    // 以**内部 snake_case key** 存(运行时只认 snake_case)。
    for key in [
        "access_token",
        "client_id",
        "client_secret",
        "region",
        "profile_arn",
        "auth_method",
    ] {
        if let Some(v) = flat_str(acc, key).filter(|s| !s.is_empty()) {
            extra.insert(key.into(), json!(v));
        }
    }

    // machineId(若带:校验/归一,非法不写——与嵌套路径同规则)。
    if let Some(mid) = acc
        .get("machine_id")
        .or_else(|| acc.get("machineId"))
        .and_then(|v| v.as_str())
    {
        if let Some(n) = crate::machine_id::normalize_machine_id(mid) {
            extra.insert("machine_id".into(), json!(n));
        }
    }

    // kiro_provider(若带:kiro_provider / provider / idp 归一)。
    let idp = flat_str(acc, "kiro_provider")
        .or_else(|| flat_str(acc, "provider"))
        .or_else(|| flat_str(acc, "idp"))
        .map(normalize_kiro_provider);
    if let Some(p) = idp.filter(|s| !s.is_empty()) {
        extra.insert("kiro_provider".into(), json!(p));
    }

    // expires_at:字符串(RFC3339)原样;数字(epoch s/ms,>1e12 视为毫秒)转 RFC3339。
    // snake/camel 都认(expires_at / expiresAt)。
    if let Some(v) = acc.get("expires_at").or_else(|| acc.get("expiresAt")) {
        if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
            extra.insert("expires_at".into(), json!(s));
        } else if let Some(n) = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)) {
            let secs = if n > 1_000_000_000_000 { n / 1000 } else { n };
            extra.insert("expires_at".into(), json!(crate::token::format_unix_utc(secs)));
        }
    }

    // kirogo 风格:无绝对 expires_at,但有 expiresIn(相对秒) + timestamp(令牌签发时刻)。
    // 绝对值优先(上面已设则跳过);否则按【签发时刻】+ expiresIn 算 expires_at。
    // 关键:必须以签发 timestamp 为基准,不能用 now —— 否则早已过期的导入 token 会被
    // 当成新鲜,首次请求拿陈旧 access_token 吃 403。timestamp 解析失败则留空(按需刷新兜底)。
    if !extra.contains_key("expires_at") {
        let rel = acc
            .get("expires_in")
            .or_else(|| acc.get("expiresIn"))
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .filter(|&r| r >= 0); // 负 expiresIn = 脏数据 → 留空,按需刷新兜底
        let ts = acc.get("timestamp").and_then(|v| v.as_str());
        if let (Some(rel), Some(ts)) = (rel, ts) {
            // checked_add 防极端 expiresIn 溢出(溢出 → 留空,不写错误时刻也不 panic)。
            if let Some(exp) =
                crate::token::parse_rfc3339_to_unix(ts).and_then(|issued| issued.checked_add(rel))
            {
                extra.insert("expires_at".into(), json!(crate::token::format_unix_utc(exp)));
            }
        }
    }

    // 展示/对账字段。
    let email = flat_str(acc, "email").filter(|s| !s.is_empty());
    if let Some(e) = email {
        extra.insert("email".into(), json!(e));
    }
    if let Some(uid) = flat_str(acc, "user_id")
        .or_else(|| flat_str(acc, "userId"))
        .filter(|s| !s.is_empty())
    {
        extra.insert("user_id".into(), json!(uid));
    }
    if let Some(nick) = flat_str(acc, "nickname").filter(|s| !s.is_empty()) {
        extra.insert("nickname".into(), json!(nick));
    }
    if let Some(title) = flat_str(acc, "subscription_title")
        .or_else(|| {
            acc.get("subscription")
                .and_then(|s| s.get("title").or_else(|| s.get("type")))
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
    {
        extra.insert("subscription_title".into(), json!(title));
    }

    // account_id:email → user_id → id,清洗成合法路径段。
    let raw_id = email
        .or_else(|| flat_str(acc, "user_id"))
        .or_else(|| flat_str(acc, "userId"))
        .or_else(|| flat_str(acc, "id"))
        .unwrap_or("kiro-account");
    let account_id = sanitize_account_id(raw_id);

    Some(ImportedAccount { account_id, extra })
}

/// 取顶层字符串字段(扁平格式用)。
fn top_str<'a>(acc: &'a Value, field: &str) -> Option<&'a str> {
    acc.get(field).and_then(|v| v.as_str())
}

/// 取顶层字符串:**snake_case 优先,回退 camelCase**。
///
/// 对齐 kiro.rs 的 `#[serde(rename_all = "camelCase")]` 行为——它的导出/导入用 camelCase
/// (`refreshToken`/`clientId`/`clientSecret`/`machineId`...),而我方内部约定是 snake_case。
/// 扁平导入两种风格都要吃,否则用户在 kiro.rs 用的 camelCase 文件导进来会丢字段。
fn flat_str<'a>(acc: &'a Value, snake: &str) -> Option<&'a str> {
    top_str(acc, snake).or_else(|| {
        let camel = snake_to_camel(snake);
        // camel == snake(无下划线,如 "region"/"email"/"provider")时不重复查,直接 None。
        if camel == snake {
            None
        } else {
            acc.get(&camel).and_then(|v| v.as_str())
        }
    })
}

/// `client_secret` → `clientSecret`。仅用于导入字段名兼容(纯 ASCII)。
fn snake_to_camel(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut upper_next = false;
    for c in snake.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.push(c.to_ascii_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
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

    /// 扁平 snake_case 数组(顶层无 accounts 包裹、字段即 extra)——用户实际格式。
    fn flat_export() -> Value {
        json!([{
            "access_token": "aoa_flat_access",
            "auth_method": "idc",
            "client_id": "NHPEspwEz8-hGjN_yUO2anVzLWVhc3QtMQ",
            "client_secret": "eyJraW_flat_secret",
            "email": "mrdev3258",
            "expires_at": "2026-06-10T10:53:33.000Z",
            "expires_in": 3600,
            "profile_arn": "arn:aws:codewhisperer:us-east-1:207377045753:profile/P7CDKWEEXXCG",
            "refresh_token": "aor_flat_refresh",
            "region": "us-east-1",
            "type": "kiro"
        }])
    }

    #[test]
    fn flat_array_maps_snake_case_fields() {
        let out = parse_accounts_export(&flat_export()).unwrap();
        assert_eq!(out.len(), 1);
        let a = &out[0];
        // account_id 由 email 清洗(已是合法字符,原样)。
        assert_eq!(a.account_id, "mrdev3258");
        assert_eq!(a.extra["refresh_token"], json!("aor_flat_refresh"));
        assert_eq!(a.extra["access_token"], json!("aoa_flat_access"));
        assert_eq!(a.extra["client_id"], json!("NHPEspwEz8-hGjN_yUO2anVzLWVhc3QtMQ"));
        assert_eq!(a.extra["client_secret"], json!("eyJraW_flat_secret"));
        assert_eq!(a.extra["region"], json!("us-east-1"));
        assert_eq!(a.extra["profile_arn"].as_str().unwrap(), "arn:aws:codewhisperer:us-east-1:207377045753:profile/P7CDKWEEXXCG");
        assert_eq!(a.extra["email"], json!("mrdev3258"));
        // auth_method 扁平格式按原值搬运(snake_case 是我方约定;idc 安全,非 external_idp)。
        assert_eq!(a.extra["auth_method"], json!("idc"));
        // expires_at 已是 RFC3339 字符串,原样保留。
        assert_eq!(a.extra["expires_at"], json!("2026-06-10T10:53:33.000Z"));
        // 无 machineId → 不写(留空按 rt 派生)。
        assert!(!a.has_machine_id());
        // type/expires_in 不进 extra(非账号字段)。
        assert!(!a.extra.contains_key("type"));
        assert!(!a.extra.contains_key("expires_in"));
    }

    #[test]
    fn flat_skips_entries_without_refresh_token() {
        let v = json!([
            {"email": "a", "access_token": "at"},
            {"email": "b", "refresh_token": "rt"}
        ]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out.len(), 1, "无 refresh_token 的扁平条目应跳过");
        assert_eq!(out[0].account_id, "b");
    }

    #[test]
    fn flat_epoch_ms_number_converts_to_rfc3339() {
        let v = json!([{"refresh_token": "rt", "expires_at": 1781121312584i64}]);
        let out = parse_accounts_export(&v).unwrap();
        let exp = out[0].extra["expires_at"].as_str().unwrap();
        assert!(exp.ends_with('Z') && exp.contains('T'), "数字 epoch 应转 RFC3339: {exp}");
    }

    #[test]
    fn accounts_wrapped_flat_objects_also_parse() {
        // { accounts: [ <扁平对象> ] } —— 无 credentials 子对象也按扁平分流。
        let v = json!({"accounts": [{"refresh_token": "rt", "email": "x", "client_id": "c", "client_secret": "s"}]});
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].extra["client_id"], json!("c"));
    }

    /// 用户在 kiro.rs 用的 **camelCase BuilderId** 导出(不封号的格式)。扁平、无
    /// `credentials` 子对象、字段 camelCase。核心回归:clientId/clientSecret 必须导进来,
    /// 否则刷新分流(看 client_id+secret 在不在)判错 → BuilderId 号坏。
    #[test]
    fn flat_camelcase_builderid_keeps_client_credentials() {
        let v = json!([{
            "email": "natalie.raymond.walker@example.com",
            "refreshToken": "aor_builderid_refresh",
            "provider": "BuilderId",
            "password": "ignored-pw",
            "clientId": "sb16AvMRqYSnm6f4-G40bHVzLWVhc3QtMQ",
            "clientSecret": "eyJraW_builderid_secret"
        }]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out.len(), 1);
        let a = &out[0];
        // camelCase 全部读到并以内部 snake_case 落库。
        assert_eq!(a.extra["refresh_token"], json!("aor_builderid_refresh"));
        assert_eq!(
            a.extra["client_id"], json!("sb16AvMRqYSnm6f4-G40bHVzLWVhc3QtMQ"),
            "clientId 必须导入,否则刷新分流判成 social"
        );
        assert_eq!(a.extra["client_secret"], json!("eyJraW_builderid_secret"));
        // provider: BuilderId → kiro_provider 归一为 builderid。
        assert_eq!(a.extra["kiro_provider"], json!("builderid"));
        assert_eq!(a.extra["email"], json!("natalie.raymond.walker@example.com"));
        // password 非账号字段,不进 extra。
        assert!(!a.extra.contains_key("password"));
    }

    /// camelCase 的 accessToken/profileArn/expiresAt/machineId 也要认。
    #[test]
    fn flat_camelcase_extra_fields_recognized() {
        let mid = "a".repeat(64);
        let v = json!([{
            "refreshToken": "rt",
            "accessToken": "at-camel",
            "profileArn": "arn:aws:codewhisperer:us-east-1:1:profile/X",
            "expiresAt": "2026-06-12T00:00:00Z",
            "machineId": mid,
            "userId": "uid-9"
        }]);
        let a = &parse_accounts_export(&v).unwrap()[0];
        assert_eq!(a.extra["access_token"], json!("at-camel"));
        assert_eq!(a.extra["profile_arn"].as_str().unwrap(), "arn:aws:codewhisperer:us-east-1:1:profile/X");
        assert_eq!(a.extra["expires_at"], json!("2026-06-12T00:00:00Z"));
        assert!(a.has_machine_id(), "camelCase machineId 应导入");
        assert_eq!(a.extra["user_id"], json!("uid-9"));
    }

    #[test]
    fn snake_to_camel_basic() {
        assert_eq!(snake_to_camel("client_secret"), "clientSecret");
        assert_eq!(snake_to_camel("refresh_token"), "refreshToken");
        assert_eq!(snake_to_camel("profile_arn"), "profileArn");
        assert_eq!(snake_to_camel("region"), "region"); // 无下划线原样
    }

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
        let out = parse_accounts_export(&enterprise_export()).unwrap();
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
        let out = parse_accounts_export(&v).unwrap();
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
        let out = parse_accounts_export(&v).unwrap();
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
        let out = parse_accounts_export(&v).unwrap();
        assert!(!out[0].has_machine_id(), "非法 machineId 不应被当成已设置");
        assert!(!out[0].extra.contains_key("machine_id"));
    }

    #[test]
    fn string_expires_at_is_parsed() {
        let v = json!({"accounts": [{
            "email": "x@y.com",
            "credentials": {"refreshToken": "rt", "accessToken": "at", "expiresAt": "1781121312584"}
        }]});
        let out = parse_accounts_export(&v).unwrap();
        let exp = out[0].extra["expires_at"].as_str().unwrap();
        assert!(exp.ends_with('Z') && exp.contains('T'), "字符串 epoch 也应转 RFC3339: {exp}");
    }

    #[test]
    fn errors_when_no_accounts_array() {
        assert!(parse_accounts_export(&json!({"version": "1.7.5"})).is_err());
        assert!(parse_accounts_export(&json!({"accounts": []})).is_err());
    }

    #[test]
    fn sanitize_account_id_rules() {
        assert_eq!(sanitize_account_id("a@b+c.com"), "a-b-c.com");
        assert_eq!(sanitize_account_id(""), "kiro-account");
        assert_eq!(sanitize_account_id(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn kirogo_flat_camelcase_with_computed_expires_at() {
        // kirogo 导出:顶层数组 + camelCase 字段 + expiresIn/timestamp(无 credentials、无 machineId)。
        let root = json!([{
            "email": "lucas17@amazon.lalicy.com",
            "accessToken": "at-xyz",
            "refreshToken": "rt-xyz",
            "idp": "BuilderId",
            "clientId": "cid",
            "clientSecret": "csecret",
            "expiresIn": 3600,
            "timestamp": "2026-06-07T18:20:46+08:00",
            "subscriptionTitle": "Pro",
            "fingerprintProfileId": "gen-1280x800-8afbf2e3"
        }]);
        let out = parse_accounts_export(&root).expect("kirogo 扁平格式应被解析");
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.account_id, "lucas17-amazon.lalicy.com");
        assert_eq!(a.extra["refresh_token"], json!("rt-xyz"));
        assert_eq!(a.extra["access_token"], json!("at-xyz"));
        assert_eq!(a.extra["client_id"], json!("cid"));
        assert_eq!(a.extra["client_secret"], json!("csecret"));
        assert_eq!(a.extra["kiro_provider"], json!("builderid"));
        assert_eq!(a.extra["subscription_title"], json!("Pro"));
        // expires_at = 签发时刻(18:20:46+08:00 = 10:20:46Z)+ expiresIn(1h)= 11:20:46Z。
        assert_eq!(a.extra["expires_at"], json!("2026-06-07T11:20:46Z"));
        // 无 machineId → 不写(运行时按当前 rt 派生 = 与真实 Kiro 客户端一致,见 machine_id.rs)。
        assert!(!a.has_machine_id());
        // fingerprintProfileId 非内部字段 → 不入 extra(machineId 走 rt 派生,不需要它)。
        assert!(!a.extra.contains_key("fingerprintProfileId"));
    }

    #[test]
    fn kirogo_bad_expiry_leaves_expires_at_unset() {
        // 负 expiresIn / 无法解析的 timestamp → 不写 expires_at(留空按需刷新),账号仍正常导入。
        for (ei, ts) in [
            (json!(-3600), "2026-06-07T18:20:46+08:00"),
            (json!(3600), "garbage-timestamp"),
        ] {
            let root = json!([{ "refreshToken": "rt", "expiresIn": ei, "timestamp": ts }]);
            let out = parse_accounts_export(&root).expect("账号仍应导入");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].extra["refresh_token"], json!("rt"));
            assert!(
                !out[0].extra.contains_key("expires_at"),
                "脏 expiry(ei={ei}, ts={ts})不应写 expires_at"
            );
        }
    }
}
