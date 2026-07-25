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

/// 纯目录区(非 Kiro 数据面区)回落用的默认服务区。已知数据面区清单见
/// [`crate::profiles::STANDARD_PROFILE_REGIONS`](us-east-1 / eu-central-1)。
const SERVICE_REGION: &str = "us-east-1";

/// 从 profileArn 提取数据面区域(`arn:aws:codewhisperer:{region}:{acct}:profile/{id}` 第 4 段)。
/// **仅**当解析出的区域是已知商业服务区([`crate::profiles::STANDARD_PROFILE_REGIONS`])才返回
/// (返回静态表里的规范值),避免脏 ARN/未知区污染服务区。
///
/// `pub`:除导入时机(本模块 [`insert_region_split`])外,gw-app 的 worker 在**运行时首次
/// 发现** profileArn 时(账号导出未带 profileArn,靠 `ListAvailableProfiles` 现查)也复用
/// 这份规则反过来修正 `region`/`api_region`——同一份真理来源,不重复实现。
pub fn region_from_profile_arn(arn: &str) -> Option<&'static str> {
    let region = arn.split(':').nth(3)?.trim();
    known_service_region(Some(region))
}

/// 命中已知服务区 → 返回静态规范值;否则 None(空/未知区)。
fn known_service_region(r: Option<&str>) -> Option<&'static str> {
    let r = r.map(str::trim).filter(|s| !s.is_empty())?;
    crate::profiles::STANDARD_PROFILE_REGIONS
        .iter()
        .find(|s| s.eq_ignore_ascii_case(r))
        .copied()
}

/// 写入区域三兄弟,支持**认证区 ≠ 服务区**的分离区域号(号商/企业 IdC 常见:SSO/OIDC 客户端
/// 注册在 us-east-1,但 CodeWhisperer profile/entitlement 在 eu-central-1)。
/// - **数据面/服务区**(`region` + `api_region`,供 getUsageLimits 与 chat 建端点):优先取
///   **profileArn 的区域**(权威——profile 在哪个区,数据面 API 就必须打哪个区),其次显式
///   `apiRegion`,再次导出 `region`(若本身已是已知服务区),兜底 us-east-1(纯目录区导出如
///   us-east-2 时数据面只在 us-east-1)。
/// - **认证/目录区**(`auth_region`,供 OIDC 刷新):`authRegion` 优先,否则导出 `region`。
///   token.rs 刷新读 `auth_region ?? region`,分离区号必须让它拿到真 SSO 区(如 us-east-1)。
fn insert_region_split(
    extra: &mut Map<String, Value>,
    exported_region: Option<&str>,
    auth_region: Option<&str>,
    api_region: Option<&str>,
    profile_arn: Option<&str>,
) {
    let dir = exported_region.map(str::trim).filter(|s| !s.is_empty());
    // 服务区:profileArn 区域 > 显式 apiRegion > 导出 region(已知服务区)> us-east-1。
    let service: &str = profile_arn
        .and_then(region_from_profile_arn)
        .or_else(|| known_service_region(api_region))
        .or_else(|| known_service_region(exported_region))
        .unwrap_or(SERVICE_REGION);
    extra.insert("region".into(), json!(service));
    // 显式写 api_region,让 chat 的 api_region() 直接命中(不依赖回落 region),口径清晰。
    extra.insert("api_region".into(), json!(service));
    // 认证区:authRegion 优先,否则导出 region(纯目录区)。
    let auth = auth_region.map(str::trim).filter(|s| !s.is_empty()).or(dir);
    if let Some(a) = auth {
        extra.insert("auth_region".into(), json!(a));
    }
}

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

/// 宽松解析导入 JSON 文本 → `Value`。先按标准严格解析;失败再把**非标准空白**
/// (不间断空格 nbsp 等,常见于从网页/富文本/PDF 复制粘贴的凭据,JSON 规范只认
/// 空格/Tab/CR/LF 四种空白,nbsp 会让 serde 直接报错)归一为 ASCII 空格后重试。
/// 仅在严格解析**失败**时才走归一兜底:合法 JSON 永不受影响;凭据值均为 ASCII,
/// 归一对数据无损。这样用户直接粘贴上号工具的原文(哪怕带 nbsp 缩进)也能导入。
pub fn parse_import_json(raw: &str) -> Result<Value, String> {
    // 1) 严格解析:整段必须恰好是一个 JSON 值。
    let strict_err = match serde_json::from_str::<Value>(raw) {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    // 2) 归一非标准 Unicode 空白(nbsp 等)后再严格解析。
    let normalized = normalize_json_whitespace(raw);
    if normalized != raw {
        if let Ok(v) = serde_json::from_str::<Value>(&normalized) {
            return Ok(v);
        }
    }
    // 3) 宽松兜底:只取文本里的**第一个完整 JSON 值**,忽略其后的尾随字符。
    //    覆盖高频误操作:从多账号文件里单独复制一个账号,尾巴带上了数组/外层的收尾括号
    //    (如 `{...}\n]\n}`)——严格解析报 "trailing characters",但前面那个账号对象本身
    //    完好可用。用流式 Deserializer 取首值、不校验输入是否恰好在此结束。
    //    这是原严格行为的**超集**:所有原本合法的整段 JSON 仍在步骤 1/2 命中,行为不变。
    if let Some(v) = parse_leading_json_value(&normalized) {
        return Ok(v);
    }
    // 4) 仍解析不出任何完整首值(如 `{"refreshToken": }` / 纯文本):回报原始严格错误。
    Err(format!("JSON 解析失败: {strict_err}"))
}

/// 解析文本中的**第一个完整 JSON 值**,忽略其后的任意尾随字符;解析不出首值返回 None。
/// 用于容忍「从多账号文件里复制单个账号」时带上的尾随 `]` / `}` 等碎片。
fn parse_leading_json_value(s: &str) -> Option<Value> {
    serde_json::Deserializer::from_str(s)
        .into_iter::<Value>()
        .next()
        .and_then(|r| r.ok())
}

/// 把非标准 Unicode 空白归一为 ASCII 空格(nbsp / 窄 nbsp / figure space / 各类
/// em-en 空格 / 零宽不换行 BOM)。仅供 [`parse_import_json`] 兜底调用。
fn normalize_json_whitespace(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{00A0}' | '\u{202F}' | '\u{2007}' | '\u{2060}' | '\u{FEFF}' => ' ',
            '\u{2000}'..='\u{200A}' => ' ',
            other => other,
        })
        .collect()
}

/// 解析账号导出。支持三种顶层形态:
/// 1. **数组** `[ {...}, ... ]`(扁平/混合);
/// 2. **`{ accounts: [...] }`** 包裹(KiroManager/混合);
/// 3. **单个账号对象** `{ refreshToken, clientId, ... }`(部分上号工具一次只给一个号,
///    既无外层数组也无 `accounts` 包裹)——视作 1 元素数组。
///
/// 数组元素可为对象或裸 `ksk_` 字符串(官方 API Key)。跳过既无 refreshToken 也无
/// kiro_api_key 的条目。全部无效返回 `Err`。
pub fn parse_accounts_export(root: &Value) -> Result<Vec<ImportedAccount>, String> {
    let accounts: &[Value] = if let Some(arr) = root.as_array() {
        arr.as_slice()
    } else if let Some(arr) = root.get("accounts").and_then(|a| a.as_array()) {
        arr.as_slice()
    } else if is_single_account_object(root) {
        // 单账号对象:借用 root 自身组成 1 元素切片,交给 map_account 按"有无 credentials"分流。
        std::slice::from_ref(root)
    } else {
        return Err(
            "JSON 既不是账号数组/含 accounts 数组,也不是单个账号对象(缺 refreshToken/credentials)"
                .to_string(),
        );
    };

    let mut out = Vec::new();
    let mut skipped = 0;
    for acc in accounts {
        match map_account(acc) {
            Some(m) => out.push(m),
            None => skipped += 1,
        }
    }
    if out.is_empty() {
        return Err(format!(
            "未解析到任何有效账号({} 条既无 refreshToken 也无 kiro_api_key 被跳过)",
            skipped
        ));
    }
    Ok(out)
}

/// 判断 root 是否是**单个账号对象**(而非误传的 `{version:...}` 之类元数据)。
/// 判据:是对象,且带 `credentials` 子对象,或带非空 refreshToken/refresh_token。
/// 严格判据避免把 `{accounts:[]}`/`{version:...}` 误当账号(它们应继续报错)。
fn is_single_account_object(root: &Value) -> bool {
    if !root.is_object() {
        return false;
    }
    let has_rt = |k: &str| {
        root.get(k)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    };
    root.get("credentials").is_some() || has_rt("refreshToken") || has_rt("refresh_token")
}

/// 单账号分流:裸字符串 → 官方 API Key([`map_api_key`]);含 `credentials` 子对象 →
/// KiroManager 嵌套([`map_one`]);否则 → 扁平 snake_case([`map_flat`],内部再识别 apikey)。
fn map_account(acc: &Value) -> Option<ImportedAccount> {
    // 数组元素是裸字符串 → 视为官方 API Key(ksk_...)条目。
    if let Some(s) = acc.as_str() {
        return map_api_key(s, None);
    }
    if acc.get("credentials").is_some() {
        map_one(acc)
    } else {
        map_flat(acc)
    }
}

/// 官方 Kiro API Key 前缀。非此形态一律不当作 apikey(防把任意粘贴串/误配值建成账号:
/// 会跳过刷新/profileArn、拿 TokenType: API_KEY、改指纹策略——审查 Skeptic#1/Architect#3)。
const API_KEY_PREFIX: &str = "ksk_";

/// 是否为形态合法的官方 API Key(`ksk_` 前缀且前缀后非空)。
pub(crate) fn is_valid_api_key(key: &str) -> bool {
    key.starts_with(API_KEY_PREFIX) && key.len() > API_KEY_PREFIX.len()
}

/// 从粘贴文本切出官方 API Key 列表(空白/逗号分隔,每行一个 `ksk_...`)。
/// **只保留 `ksk_` 前缀**的合法 key,去重(保序),丢弃空白/非法项。
pub fn split_api_keys(raw: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for token in raw.split(|c: char| c.is_whitespace() || c == ',') {
        let key = token.trim();
        if !is_valid_api_key(key) || !seen.insert(key.to_string()) {
            continue;
        }
        out.push(key.to_string());
    }
    out
}

/// 官方 Kiro API Key(`ksk_` 开头)→ apikey 账号。`acc` 为携带该 key 的同条目(可选),
/// 用于顺带搬运 email/region/subscription/machine_id 等展示字段。
///
/// 与 social/IdC 的差别:**不需要** refresh_token / profileArn / access_token。发包时 bearer
/// 直接取 `kiro_api_key`(见 [`crate::headers::bearer_token`]),worker `has_fresh_token` 对
/// apikey 恒真(不触发刷新)。鉴权靠出站 `TokenType: API_KEY` 头,`auth_method` 仅作 UI 标签。
pub(crate) fn map_api_key(key: &str, acc: Option<&Value>) -> Option<ImportedAccount> {
    let key = key.trim();
    // 必须是形态合法的 ksk_ key,否则不建号(裸串路径与 JSON 路径共用此闸)。
    if !is_valid_api_key(key) {
        return None;
    }
    let mut extra = Map::new();
    extra.insert("kiro_api_key".into(), json!(key));
    extra.insert("auth_method".into(), json!("api_key"));

    let mut email: Option<&str> = None;
    if let Some(acc) = acc {
        if let Some(region) = flat_str(acc, "region").filter(|s| !s.is_empty()) {
            extra.insert("region".into(), json!(region));
        }
        email = flat_str(acc, "email").filter(|s| !s.is_empty());
        if let Some(e) = email {
            extra.insert("email".into(), json!(e));
        }
        if let Some(uid) = flat_str(acc, "user_id")
            .or_else(|| flat_str(acc, "userId"))
            .filter(|s| !s.is_empty())
        {
            extra.insert("user_id".into(), json!(uid));
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
        if let Some(mid) = acc
            .get("machine_id")
            .or_else(|| acc.get("machineId"))
            .and_then(|v| v.as_str())
        {
            if let Some(n) = crate::machine_id::normalize_machine_id(mid) {
                extra.insert("machine_id".into(), json!(n));
            }
        }
    }

    // account_id:有 email 用 email(可读),否则由**整个 key 的 sha256 前 12 位十六进制**派生
    // ——抗碰撞(48-bit,远胜尾 8 字符的 ~32-bit)、按字符切绝不 panic、且不泄露 key 本身
    // (审查 Skeptic#1/#2、Architect#4)。
    let account_id = match email {
        Some(e) => sanitize_account_id(e),
        None => format!("kiro-apikey-{}", &crate::machine_id::sha256_hex(key)[..12]),
    };
    Some(ImportedAccount { account_id, extra })
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
    // 区域三兄弟(支持认证区≠服务区的分离区号):服务区优先取 profileArn 区域,
    // 认证区取 authRegion/region。见 insert_region_split。
    let arn = str_at(creds, "profileArn").filter(|s| !s.is_empty());
    insert_region_split(
        &mut extra,
        str_at(creds, "region"),
        str_at(creds, "authRegion"),
        str_at(creds, "apiRegion"),
        arn,
    );
    if let Some(arn) = arn {
        extra.insert("profile_arn".into(), json!(arn));
    }

    // 2b. External IdP(Azure AD 租户)刷新材料。**只有** credentials.authMethod 明确等于
    //     "external_idp" 才映射 auth_method——其余值(KiroManager 的 "IdC"/"Enterprise" 等)
    //     继续沿用第 3 步的"不映射"策略,避免误触发 TokenType: EXTERNAL_IDP 头。
    //     Kiro-Go 自身的 external_idp 导出恰好会带 authMethod="external_idp"(其
    //     proxy/handler.go apiExportAccounts 对该值不做大小写重写,原值透传)。
    if str_at(creds, "authMethod").is_some_and(|v| v.trim().eq_ignore_ascii_case("external_idp")) {
        extra.insert("auth_method".into(), json!("external_idp"));
        // token_endpoint/scope:优先显式字段,否则从 userId + clientId 派生(逻辑与
        // 白名单校验在 resolve_external_idp_refresh_material 里统一实现,map_flat
        // 共用同一份,避免两条导入路径各写一份容易长歪的重复代码)。
        let (token_endpoint, scope) = resolve_external_idp_refresh_material(
            str_at(creds, "tokenEndpoint"),
            str_at(creds, "scopes").or_else(|| str_at(creds, "scope")),
            acc.get("userId").and_then(|v| v.as_str()),
            str_at(creds, "clientId"),
        );
        if let Some(te) = token_endpoint {
            extra.insert("token_endpoint".into(), json!(te));
        }
        if let Some(sc) = scope {
            extra.insert("scope".into(), json!(sc));
        }
    }

    // 3. 身份来源 kiro_provider:供 profileArn 兜底 + internal 头判定。
    //    取 credentials.provider,回退顶层 idp。**不**映射 authMethod(IdC 分流看
    //    client_id+secret;authMethod=external_idp 会误触发 TokenType 头)——除
    //    external_idp 本身外(第 2b 步已单独处理,那是本次要修的能力,不是要避免的误判)。
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
    if let Some(title) = subscription_title_of(acc) {
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
    // 官方 API Key 凭据优先:带 kiro_api_key / kiroApiKey / api_key / apiKey 即走 apikey 路径
    // (不需要 refresh_token)。
    if let Some(key) = flat_str(acc, "kiro_api_key")
        .or_else(|| flat_str(acc, "api_key"))
        .filter(|s| !s.is_empty())
    {
        return map_api_key(key, Some(acc));
    }
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
        "profile_arn",
        "auth_method",
    ] {
        if let Some(v) = flat_str(acc, key).filter(|s| !s.is_empty()) {
            extra.insert(key.into(), json!(v));
        }
    }
    // 区域三兄弟(支持认证区≠服务区的分离区号)。profile_arn 已在上面的循环里写入 extra;
    // 这里从原始 acc 读 apiRegion/authRegion(flat_str snake→camel 兼容号商 camelCase 导出)。
    insert_region_split(
        &mut extra,
        flat_str(acc, "region"),
        flat_str(acc, "auth_region"),
        flat_str(acc, "api_region"),
        flat_str(acc, "profile_arn"),
    );
    // token_endpoint/scope 不在上面的通用循环里处理——它们需要 external_idp 专属的
    // 白名单校验 + user_id 派生兜底,统一走 resolve_external_idp_refresh_material
    // (下方,在 user_id 写入 extra 之后调用),与嵌套路径共用同一份逻辑。

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

    // external_idp 账号:token_endpoint/scope 优先显式字段(camelCase/snake_case 均认,
    // 复数拼写 "scopes" 也认),否则从 user_id + client_id 派生并过白名单——与嵌套
    // KiroManager 路径(map_one 的 2b 步)共用同一份 resolve_external_idp_refresh_material。
    if extra
        .get("auth_method")
        .and_then(|v| v.as_str())
        .is_some_and(|m| m.eq_ignore_ascii_case("external_idp"))
    {
        let (token_endpoint, scope) = resolve_external_idp_refresh_material(
            flat_str(acc, "token_endpoint"),
            flat_str(acc, "scope").or_else(|| top_str(acc, "scopes")),
            extra.get("user_id").and_then(|v| v.as_str()),
            extra.get("client_id").and_then(|v| v.as_str()),
        );
        if let Some(te) = token_endpoint {
            extra.insert("token_endpoint".into(), json!(te));
        }
        if let Some(sc) = scope {
            extra.insert("scope".into(), json!(sc));
        }
    }

    if let Some(nick) = flat_str(acc, "nickname").filter(|s| !s.is_empty()) {
        extra.insert("nickname".into(), json!(nick));
    }
    if let Some(title) = subscription_title_of(acc) {
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
/// 提取订阅档名(供 profileArn 自愈的**付费闸门** + 模型过滤 + 展示)。兼容多种导出结构:
/// ① 扁平 `subscription_title`(snake/camel);② `subscription.{title,type}`;
/// ③ `usageData.subscriptionInfo.subscriptionTitle`(**KiroManager 桌面版真实结构**)。
/// 缺失 → None。
///
/// 之所以必须认 ③:KiroManager 桌面版把订阅档嵌在 `usageData.subscriptionInfo` 里,只认
/// ①② 会让付费 builderid 号导入后 `subscription_title` 为空 → 付费闸门当它是免费号、跳过
/// profileArn 强制发现 → 用错免费共享 ARN → getUsageLimits/chat 403 → 被误禁。构成死锁
/// (发现需订阅档 / 订阅档靠成功配额回填 / 配额需正确 profileArn),导入时就填上即可断链。
///
/// ③ **只信 `subscriptionTitle`,不兜底 `type`**:该字段同时喂两个用 `!contains("FREE")`
/// 的脆弱闸门(付费发现 + opus 能力过滤),而人读标题("KIRO POWER"/"KIRO FREE")可靠含
/// FREE 字样、机器枚举 `type`(如 `Q_DEVELOPER_STANDALONE_POWER`)的免费档字面未证实含
/// FREE——用 type 兜底有把免费号误判成付费的风险(放行 opus→403 误禁 + 污染免费 ARN)。
/// 且与 runtime `usage_limits.rs::SubscriptionInfo`(只认 subscriptionTitle)口径一致。
fn subscription_title_of(acc: &Value) -> Option<&str> {
    flat_str(acc, "subscription_title")
        .or_else(|| {
            acc.get("subscription")
                .and_then(|s| s.get("title").or_else(|| s.get("type")))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            acc.get("usageData")
                .and_then(|u| u.get("subscriptionInfo"))
                .and_then(|si| si.get("subscriptionTitle"))
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
}

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

/// 从 `userId`(external_idp 账号形如
/// `https://login.microsoftonline.com/{tenant}/v2.0.{oid}`,或裸的 issuer URL
/// `https://login.microsoftonline.com/{tenant}/v2.0`)派生 Azure AD token endpoint。
/// 对齐 Kiro-Go `auth/kiro_sso.go` 的 `DeriveExternalIdpEndpoints`:取 URL path 第一段
/// 当租户 ID,拼 `{scheme}://{host}/{tenant}/oauth2/v2.0/token`。解析失败(非 URL / 无
/// path 段)返回 `None`——调用方据此跳过,账号仍会正常导入,只是刷新会因缺
/// token_endpoint 报错,需运维事后在 admin 手动补(优于让整条导入失败)。
fn derive_external_idp_token_endpoint(user_id: &str) -> Option<String> {
    // 用真正的 URL 解析(而非手写 split)取 path 的第一段当租户 ID——手写
    // `split_once`/`splitn` 版本不会剥离 `?query`/`#fragment`,一份带 query 的 userId
    // 会把垃圾拼进 tenant 段、产出语法错误的 token_endpoint(审查发现的真实 bug)。
    let parsed = url::Url::parse(user_id.trim()).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    let tenant = parsed.path_segments()?.next().filter(|s| !s.is_empty())?;
    Some(format!("{scheme}://{host}/{tenant}/oauth2/v2.0/token"))
}

/// external_idp 账号未显式带 scope 时的默认作用域。对齐 Kiro-Go
/// `DeriveExternalIdpEndpoints`:两个 codewhisperer scope 加 `offline_access`
/// (`offline_access` 是拿到 refresh_token 的必要 scope,少了它 Azure AD 授权码
/// 交换不会下发 refresh_token)。
fn default_external_idp_scope(client_id: &str) -> String {
    format!(
        "api://{client_id}/codewhisperer:conversations api://{client_id}/codewhisperer:completions offline_access"
    )
}

/// external_idp(Azure AD 租户)刷新材料的统一解析:token_endpoint/scope 优先取显式
/// 字段,否则从 user_id + client_id 派生。嵌套(map_one)和扁平(map_flat)两条导入
/// 路径共用同一份逻辑,避免各写一份容易长歪的重复代码(审查发现的重复)。
///
/// token_endpoint(不论显式还是派生)都要过 [`crate::token::validate_external_idp_endpoint`]
/// 白名单——校验不过就丢弃、返回 `None`(账号仍正常导入,只是缺 token_endpoint,留给
/// 运维在 admin 补;不能让一份被篡改/半可信的导出文件把服务器的 refresh_token POST
/// 到攻击者主机——审查发现的高危缺口)。
fn resolve_external_idp_refresh_material(
    explicit_token_endpoint: Option<&str>,
    explicit_scope: Option<&str>,
    user_id: Option<&str>,
    client_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    let token_endpoint = explicit_token_endpoint
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| user_id.and_then(derive_external_idp_token_endpoint))
        .filter(|te| crate::token::validate_external_idp_endpoint(te).is_ok());

    let scope = explicit_scope
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| client_id.filter(|s| !s.is_empty()).map(default_external_idp_scope));

    (token_endpoint, scope)
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
        assert_eq!(a.extra["api_region"], json!("us-east-1"), "非分离号 api_region=服务区");
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
        assert_eq!(a.extra["api_region"], json!("us-east-1"), "非分离号 api_region=服务区");
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

    /// KiroManager 桌面版导出:扁平账号对象(无 credentials 子对象、无顶层 subscription),
    /// 订阅档嵌在 `usageData.subscriptionInfo.subscriptionTitle`。回归:此前只认扁平
    /// `subscription_title` 与 `subscription.{title,type}`,漏掉此路径 → 付费 builderid 号
    /// 导入后 subscription_title 为空 → profileArn 自愈的付费闸门死锁(getUsageLimits/chat
    /// 403 → 误禁健康付费号)。见 [`subscription_title_of`] 与 caio-idc-dead-account 记忆。
    #[test]
    fn kiromanager_desktop_subscription_from_usage_data() {
        let export = json!([{
            "email": "awspofig3816@gmail.com",
            "refreshToken": "rt_xxx",
            "accessToken": "at_xxx",
            "clientId": "cid",
            "clientSecret": "cs",
            "region": "us-east-1",
            "provider": "BuilderId",
            "authMethod": "IdC",
            "profileArn": null,
            "usageData": {
                "subscriptionInfo": {
                    "subscriptionTitle": "KIRO POWER",
                    "type": "Q_DEVELOPER_STANDALONE_POWER"
                }
            }
        }]);
        let out = parse_accounts_export(&export).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].extra["subscription_title"],
            json!("KIRO POWER"),
            "必须从 usageData.subscriptionInfo.subscriptionTitle 提取,否则付费闸门死锁"
        );
        // 付费 builderid 号:kiro_provider=builderid、无 profile_arn(留给运行时自愈发现)。
        assert_eq!(out[0].extra["kiro_provider"], json!("builderid"));
        assert!(!out[0].extra.contains_key("profile_arn"), "profileArn=null 不应写入");
    }

    /// us-east-2 IdC 企业号(KiroManager 桌面扁平导出):目录区 us-east-2 必须落 `auth_region`
    /// (OIDC 刷新走 oidc.us-east-2.amazonaws.com),而服务区 `region` 恒置 us-east-1——否则
    /// getUsageLimits 拼 `q.us-east-2.amazonaws.com`、chat 拼 `runtime.us-east-2.kiro.dev`
    /// (均 NXDOMAIN)→ 网络层失败(实测导入 3816 同批号 quota 502)。见 insert_region_split
    /// 与 caio-idc-dead-account 记忆「第四刀:region 拆分」。
    #[test]
    fn flat_idc_us_east_2_splits_directory_region_to_auth_region() {
        let export = json!([{
            "email": "HYL08@d-9a675b2b69.local",
            "refreshToken": "aor_us_east_2_refresh",
            "accessToken": "aoa_at",
            "clientId": "cT4xLY5D9a48sjFLMnSRY3VzLWVhc3QtMg",
            "clientSecret": "eyJraW_secret",
            "region": "us-east-2",
            "provider": "Enterprise",
            "authMethod": "IdC",
            "profileArn": null,
            "usageData": { "subscriptionInfo": { "subscriptionTitle": "KIRO POWER" } }
        }]);
        let out = parse_accounts_export(&export).unwrap();
        assert_eq!(out.len(), 1);
        let a = &out[0];
        // 服务区恒 us-east-1(数据面 q./runtime/ListAvailableProfiles 只在此区)。
        assert_eq!(a.extra["region"], json!("us-east-1"), "service region 必须 us-east-1");
        // 目录区落 auth_region 供 OIDC 刷新(token.rs auth_region() 读 auth_region ?? region)。
        assert_eq!(a.extra["auth_region"], json!("us-east-2"), "目录区必须进 auth_region");
        // 企业 IdC:kiro_provider=enterprise、无固定 profileArn(留运行时发现)。
        assert_eq!(a.extra["kiro_provider"], json!("enterprise"));
        assert!(!a.extra.contains_key("profile_arn"), "profileArn=null 不应写入");
        // 付费闸门靠它(走 usageData.subscriptionInfo.subscriptionTitle)。
        assert_eq!(a.extra["subscription_title"], json!("KIRO POWER"));
        // 扁平路径 auth_method 按原值搬运(map_flat 约定):KiroManager 桌面的 "IdC" 原样落库。
        // 惰性无害——仅 auth_method=="external_idp" 才触发 TokenType: EXTERNAL_IDP 头,"IdC" 不会;
        // IdC 刷新分流看 client_id+secret 而非 auth_method。
        assert_eq!(a.extra["auth_method"], json!("IdC"));
        assert_eq!(a.extra["client_id"], json!("cT4xLY5D9a48sjFLMnSRY3VzLWVhc3QtMg"));
    }

    /// 锁定 Finding 1:导出 region 本身是**已知 Kiro 服务区**(eu-central-1,数据面真实存在)时
    /// **不得**被强改成 us-east-1——否则 EU 数据面号的 quota/chat 会被打到错误区。此时 region 沿用
    /// eu-central-1,auth_region 也记 eu-central-1(刷新同区)。对照 us-east-2(纯目录区)才回落。
    #[test]
    fn flat_known_service_region_eu_central_1_is_preserved() {
        let export = json!([{
            "email": "eu-user@example.com",
            "refreshToken": "rt_eu",
            "clientId": "cid",
            "clientSecret": "cs",
            "region": "eu-central-1",
            "provider": "Enterprise",
            "authMethod": "IdC"
        }]);
        let out = parse_accounts_export(&export).unwrap();
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.extra["region"], json!("eu-central-1"), "已知服务区必须保留,勿改 us-east-1");
        assert_eq!(a.extra["auth_region"], json!("eu-central-1"));
    }

    #[test]
    fn region_from_profile_arn_parses_known_regions_only() {
        assert_eq!(
            region_from_profile_arn("arn:aws:codewhisperer:eu-central-1:1:profile/X"),
            Some("eu-central-1")
        );
        assert_eq!(
            region_from_profile_arn("arn:aws:codewhisperer:us-east-1:1:profile/X"),
            Some("us-east-1")
        );
        // 未知区(非 STANDARD_PROFILE_REGIONS)/脏 ARN → None(不污染服务区)。
        assert_eq!(
            region_from_profile_arn("arn:aws:codewhisperer:ap-southeast-1:1:profile/X"),
            None
        );
        assert_eq!(region_from_profile_arn("garbage"), None);
    }

    /// ⭐分离区域号(认证区≠服务区):号商扁平导出,SSO/OIDC 在 us-east-1(clientId 注册区),
    /// 但 CodeWhisperer profile 在 eu-central-1。服务区(region+api_region)须取 profileArn 区域
    /// eu-central-1(否则 getUsageLimits/chat 打错区 → 403),认证区 auth_region=us-east-1(否则
    /// OIDC 刷新打错区)。对齐 caio-split-region-account-import 记忆:免去逐个手工 PATCH。
    #[test]
    fn flat_split_region_derives_service_from_profile_arn() {
        let export = json!([{
            "email": "HYL02@d-906674a867.local",
            "refreshToken": "rt_split",
            "clientId": "cid",
            "clientSecret": "cs",
            "region": "us-east-1",
            "authRegion": "us-east-1",
            "apiRegion": "eu-central-1",
            "authMethod": "IdC",
            "profileArn": "arn:aws:codewhisperer:eu-central-1:447580526192:profile/HDQ4NHA4KK9Q"
        }]);
        let a = &parse_accounts_export(&export).unwrap()[0];
        assert_eq!(a.extra["region"], json!("eu-central-1"), "服务区取 profileArn 区域");
        assert_eq!(a.extra["api_region"], json!("eu-central-1"), "api_region 显式=服务区");
        assert_eq!(a.extra["auth_region"], json!("us-east-1"), "认证区取 authRegion(SSO 注册区)");
        assert_eq!(
            a.extra["profile_arn"].as_str().unwrap(),
            "arn:aws:codewhisperer:eu-central-1:447580526192:profile/HDQ4NHA4KK9Q"
        );
    }

    /// KiroManager 嵌套(credentials 子对象)分离区号:同样从 profileArn 派生服务区。
    #[test]
    fn nested_kiromanager_split_region_from_profile_arn() {
        let export = json!({
            "accounts": [{
                "email": "eu-nested@example.com",
                "credentials": {
                    "refreshToken": "rt",
                    "clientId": "cid",
                    "clientSecret": "cs",
                    "region": "us-east-1",
                    "authRegion": "us-east-1",
                    "apiRegion": "eu-central-1",
                    "authMethod": "IdC",
                    "profileArn": "arn:aws:codewhisperer:eu-central-1:1:profile/X"
                }
            }]
        });
        let a = &parse_accounts_export(&export).unwrap()[0];
        assert_eq!(a.extra["region"], json!("eu-central-1"));
        assert_eq!(a.extra["api_region"], json!("eu-central-1"));
        assert_eq!(a.extra["auth_region"], json!("us-east-1"));
    }

    /// 锁定:`usageData.subscriptionInfo` 只带机器枚举 `type`(无人读 `subscriptionTitle`)时,
    /// **不**提取为 subscription_title。理由:该字段喂两个 `!contains("FREE")` 脆弱闸门(付费
    /// 发现 + opus 过滤),而免费档 `type` 字面是否含 FREE 未经证实——只信人读标题 subscriptionTitle
    /// (可靠含 "KIRO FREE"),缺失即按免费保守处理,避免把免费号误判成付费。
    #[test]
    fn usage_data_type_only_is_not_taken_as_subscription() {
        let export = json!([{
            "email": "free-user@gmail.com",
            "refreshToken": "rt",
            "provider": "BuilderId",
            "usageData": { "subscriptionInfo": { "type": "Q_DEVELOPER_STANDALONE_POWER" } }
        }]);
        let out = parse_accounts_export(&export).unwrap();
        assert_eq!(out.len(), 1);
        assert!(
            !out[0].extra.contains_key("subscription_title"),
            "usageData 只信 subscriptionTitle,绝不从机器枚举 type 兜底提取"
        );
    }

    /// Kiro-Go 真实导出的 external_idp(Azure AD)账号:authMethod="external_idp",
    /// 带 userId(Azure 租户 URL)+ clientId,但**不带** tokenEndpoint/scopes(Kiro-Go
    /// 自己的原生导出接口目前就是这样——这是本次要修的真实回归场景,数据取自一次
    /// 真实导出核对过的样例)。
    #[test]
    fn nested_external_idp_derives_token_endpoint_and_scope_from_user_id() {
        let v = json!({"accounts": [{
            "email": "gijs.norrman@mrdev.cyou",
            "userId": "https://login.microsoftonline.com/1f44574f-f8aa-40cf-8e43-e6bff9b4298a/v2.0.ef09475c-03d4-4ace-bcea-884e4014bd1f",
            "machineId": "7b82daf3-c8e8-4496-bc90-dea95d3e72cd",
            "credentials": {
                "accessToken": "at",
                "refreshToken": "rt",
                "clientId": "e491fadf-0239-44f9-be3b-d3e1ff193c79",
                "authMethod": "external_idp",
                "provider": "AzureAD",
                "region": "us-east-1",
                "profileArn": "arn:aws:codewhisperer:us-east-1:904962390873:profile/CQRAXYDP9YVD"
            },
            "subscription": {"type": "Pro", "title": "KIRO PRO MAX"}
        }]});
        let out = parse_accounts_export(&v).unwrap();
        let a = &out[0];
        assert_eq!(a.extra["auth_method"], json!("external_idp"));
        assert_eq!(
            a.extra["token_endpoint"],
            json!("https://login.microsoftonline.com/1f44574f-f8aa-40cf-8e43-e6bff9b4298a/oauth2/v2.0/token")
        );
        assert_eq!(
            a.extra["scope"],
            json!("api://e491fadf-0239-44f9-be3b-d3e1ff193c79/codewhisperer:conversations api://e491fadf-0239-44f9-be3b-d3e1ff193c79/codewhisperer:completions offline_access")
        );
        // profile_arn/client_id/region 等既有映射不受影响。
        assert_eq!(a.extra["client_id"], json!("e491fadf-0239-44f9-be3b-d3e1ff193c79"));
        assert!(a.extra.contains_key("profile_arn"));
    }

    /// 显式 tokenEndpoint/scopes 优先于派生值——万一导出方以后补上了这两个字段。
    #[test]
    fn nested_external_idp_prefers_explicit_token_endpoint_over_derived() {
        let v = json!({"accounts": [{
            "userId": "https://login.microsoftonline.com/tenant-a/v2.0.oid",
            "credentials": {
                "refreshToken": "rt",
                "clientId": "cid",
                "authMethod": "external_idp",
                "tokenEndpoint": "https://login.microsoftonline.com/tenant-b/oauth2/v2.0/token",
                "scopes": "custom-scope"
            }
        }]});
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(
            out[0].extra["token_endpoint"],
            json!("https://login.microsoftonline.com/tenant-b/oauth2/v2.0/token")
        );
        assert_eq!(out[0].extra["scope"], json!("custom-scope"));
    }

    /// 攻击场景本尊:一份被篡改/伪造的导出文件把 tokenEndpoint 指向攻击者主机。
    /// 必须整个丢弃这个字段(账号仍导入,只是缺 token_endpoint),绝不能把它原样写进
    /// extra——写进去就等于把这个账号未来每次刷新的 refresh_token POST 给攻击者。
    #[test]
    fn nested_external_idp_rejects_non_allowlisted_explicit_token_endpoint() {
        let v = json!({"accounts": [{
            "credentials": {
                "refreshToken": "rt",
                "clientId": "cid",
                "authMethod": "external_idp",
                "tokenEndpoint": "https://attacker.example/collect"
            }
        }]});
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out.len(), 1, "账号本身仍应导入成功");
        assert!(!out[0].extra.contains_key("token_endpoint"), "恶意 endpoint 不得写入 extra");
        // refresh_token 等其他合法字段不受牵连。
        assert_eq!(out[0].extra["refresh_token"], json!("rt"));
    }

    /// 同一场景的派生兜底路径:userId 本身不可信,若被derive成非白名单主机同样要拒绝
    /// (即使 derive_external_idp_token_endpoint 自己拼出的 host 恰好合法,这里额外验证
    /// 白名单在"派生"分支也生效,不是只挡"显式"分支)。
    #[test]
    fn nested_external_idp_derivation_also_goes_through_allowlist() {
        let v = json!({"accounts": [{
            "userId": "https://attacker.example/tenant/v2.0.oid",
            "credentials": {"refreshToken": "rt", "clientId": "cid", "authMethod": "external_idp"}
        }]});
        let out = parse_accounts_export(&v).unwrap();
        assert!(!out[0].extra.contains_key("token_endpoint"));
    }

    /// 回归测试:userId 带 query string 时,旧的手写 split 实现会把 "?foo=bar" 原样拼进
    /// tenant 段、产出语法错误的 token_endpoint;改用 url::Url 解析后必须干净剥离。
    #[test]
    fn nested_external_idp_userid_with_query_string_does_not_produce_malformed_endpoint() {
        let v = json!({"accounts": [{
            "userId": "https://login.microsoftonline.com/tenant-q/v2.0?foo=bar",
            "credentials": {"refreshToken": "rt", "clientId": "cid", "authMethod": "external_idp"}
        }]});
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(
            out[0].extra["token_endpoint"],
            json!("https://login.microsoftonline.com/tenant-q/oauth2/v2.0/token"),
            "不应含 ?query 残留"
        );
    }

    /// authMethod 不是 external_idp(如 KiroManager 的 "IdC")时,派生逻辑绝不触发——
    /// 与既有的"不映射 authMethod"策略保持一致,不能因为这次改动误伤其他账号类型。
    #[test]
    fn nested_non_external_idp_auth_method_still_not_mapped() {
        let v = json!({"accounts": [{
            "userId": "https://login.microsoftonline.com/tenant/v2.0.oid",
            "credentials": {"refreshToken": "rt", "clientId": "c", "clientSecret": "s", "authMethod": "IdC"}
        }]});
        let out = parse_accounts_export(&v).unwrap();
        assert!(!out[0].extra.contains_key("auth_method"));
        assert!(!out[0].extra.contains_key("token_endpoint"));
        assert!(!out[0].extra.contains_key("scope"));
    }

    /// 扁平格式:显式 tokenEndpoint(camelCase)直接读入,无需派生。
    #[test]
    fn flat_external_idp_reads_explicit_token_endpoint_and_scopes() {
        let v = json!([{
            "refresh_token": "rt",
            "auth_method": "external_idp",
            "clientId": "cid",
            "tokenEndpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
            "scopes": "api://cid/codewhisperer:conversations offline_access"
        }]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(
            out[0].extra["token_endpoint"],
            json!("https://login.microsoftonline.com/tenant/oauth2/v2.0/token")
        );
        assert_eq!(out[0].extra["scope"], json!("api://cid/codewhisperer:conversations offline_access"));
    }

    /// 扁平格式:无显式 tokenEndpoint,但有 userId → 派生兜底同样生效。
    #[test]
    fn flat_external_idp_derives_token_endpoint_from_user_id() {
        let v = json!([{
            "refresh_token": "rt",
            "auth_method": "external_idp",
            "client_id": "cid",
            "userId": "https://login.microsoftonline.com/tenant-x/v2.0.oid"
        }]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(
            out[0].extra["token_endpoint"],
            json!("https://login.microsoftonline.com/tenant-x/oauth2/v2.0/token")
        );
        assert_eq!(
            out[0].extra["scope"],
            json!("api://cid/codewhisperer:conversations api://cid/codewhisperer:completions offline_access")
        );
    }

    #[test]
    fn derive_external_idp_token_endpoint_cases() {
        assert_eq!(
            derive_external_idp_token_endpoint(
                "https://login.microsoftonline.com/1f44574f-f8aa-40cf-8e43-e6bff9b4298a/v2.0.ef09475c-03d4-4ace-bcea-884e4014bd1f"
            ),
            Some("https://login.microsoftonline.com/1f44574f-f8aa-40cf-8e43-e6bff9b4298a/oauth2/v2.0/token".to_string())
        );
        // 裸 issuer(无 .oid 后缀)也能拿到租户段。
        assert_eq!(
            derive_external_idp_token_endpoint("https://login.microsoftonline.com/tenant-only/v2.0"),
            Some("https://login.microsoftonline.com/tenant-only/oauth2/v2.0/token".to_string())
        );
        // 非 URL / 空 / 无 path 段 → None,不 panic。
        assert_eq!(derive_external_idp_token_endpoint(""), None);
        assert_eq!(derive_external_idp_token_endpoint("not a url"), None);
        assert_eq!(derive_external_idp_token_endpoint("https://login.microsoftonline.com"), None);
        assert_eq!(derive_external_idp_token_endpoint("https://login.microsoftonline.com/"), None);
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

    /// 用户实际格式:**单个**账号对象(无外层数组、无 accounts 包裹),camelCase 扁平字段
    /// + authMethod="IdC" + provider="BuilderId"。核心回归:必须能解析(不再报"不是支持的格式")。
    #[test]
    fn single_bare_object_builderid_idc_parses() {
        let v = json!({
            "email": "henrywood87838@outlook.de",
            "refreshToken": "aor_single_refresh",
            "clientId": "Win4BDl2_nmMaQg1yQBK3wE8gP6Ytwdd",
            "clientSecret": "eyJraW_single_secret",
            "region": "us-east-1",
            "authMethod": "IdC",
            "provider": "BuilderId"
        });
        let out = parse_accounts_export(&v).expect("单账号对象应被解析");
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.account_id, "henrywood87838-outlook.de");
        assert_eq!(a.extra["refresh_token"], json!("aor_single_refresh"));
        assert_eq!(a.extra["client_id"], json!("Win4BDl2_nmMaQg1yQBK3wE8gP6Ytwdd"));
        assert_eq!(a.extra["client_secret"], json!("eyJraW_single_secret"));
        assert_eq!(a.extra["region"], json!("us-east-1"));
        assert_eq!(a.extra["kiro_provider"], json!("builderid"));
        assert_eq!(a.extra["email"], json!("henrywood87838@outlook.de"));
        // authMethod="IdC" 经扁平路径落为 auth_method="IdC":既非 external_idp 也非 api_key,
        // 运行时不会误触发 TokenType 头(headers::is_external_idp 大小写不敏感比 external_idp)。
        assert_eq!(a.extra["auth_method"], json!("IdC"));
        // 无 machineId → 不写(BuilderId 按 rt 派生)。
        assert!(!a.has_machine_id());
    }

    /// 单个账号对象但带 `credentials` 子对象(KiroManager 单账号导出)→ 走嵌套映射。
    #[test]
    fn single_bare_object_with_credentials_parses() {
        let v = json!({
            "email": "x@y.com",
            "idp": "BuilderId",
            "credentials": {"refreshToken": "rt-nested", "clientId": "c", "clientSecret": "s"}
        });
        let out = parse_accounts_export(&v).expect("带 credentials 的单账号对象应被解析");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].extra["refresh_token"], json!("rt-nested"));
        assert_eq!(out[0].extra["client_id"], json!("c"));
        assert_eq!(out[0].extra["kiro_provider"], json!("builderid"));
    }

    /// 单对象但缺 refreshToken/credentials(误传配置/元数据)→ 仍报错,不静默吞掉。
    #[test]
    fn single_object_without_rt_or_credentials_errors() {
        assert!(parse_accounts_export(&json!({"email": "x@y.com", "region": "us-east-1"})).is_err());
    }

    /// 合法 JSON 严格解析直接成功,不经归一兜底。
    #[test]
    fn parse_import_json_accepts_strict_json() {
        let v = parse_import_json(r#"{"refreshToken":"rt","clientId":"c"}"#).unwrap();
        assert_eq!(v["refreshToken"], json!("rt"));
    }

    /// 用户实际粘贴:**单个对象 + nbsp 缩进**(从富文本复制)。严格 serde 会报错,
    /// 归一兜底后可解析。端到端再过 parse_accounts_export 应得 1 个账号。
    #[test]
    fn parse_import_json_tolerates_nbsp_indentation() {
        // \u{00A0} = 不间断空格;模拟 "email":   "...",缩进也用 nbsp。
        let raw = "{\n\u{00A0}\u{00A0}\"email\":\u{00A0}\"henrywood87838@outlook.de\",\n\
                   \u{00A0}\u{00A0}\"refreshToken\":\u{00A0}\"aor_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\",\n\
                   \u{00A0}\u{00A0}\"clientId\":\u{00A0}\"Win4BDl2\",\n\
                   \u{00A0}\u{00A0}\"provider\":\u{00A0}\"BuilderId\"\n}";
        // 严格解析必失败(nbsp 非法空白),证明兜底确有必要。
        assert!(serde_json::from_str::<Value>(raw).is_err(), "nbsp 应让严格解析失败");
        let root = parse_import_json(raw).expect("归一兜底应解析成功");
        let out = parse_accounts_export(&root).expect("应解析出账号");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].account_id, "henrywood87838-outlook.de");
        assert_eq!(out[0].extra["client_id"], json!("Win4BDl2"));
        assert_eq!(out[0].extra["kiro_provider"], json!("builderid"));
    }

    /// 真正坏的 JSON(无非标准空白可归一)→ 报原始严格错误,不假装成功。
    #[test]
    fn parse_import_json_rejects_truly_malformed() {
        assert!(parse_import_json(r#"{"refreshToken": }"#).is_err());
        assert!(parse_import_json("not json at all").is_err());
    }

    #[test]
    fn parse_import_json_recovers_leading_object_with_trailing_brackets() {
        // 从多账号文件里单独复制一个账号时,尾巴常带上数组/外层的收尾括号 `]` `}`,
        // 严格 JSON 报 trailing characters(前端旧行为=「JSON 解析失败」)。兜底应取回
        // 前面那个完好的账号对象,并顺利走通单账号导入路径。
        let raw = "{\n  \"refreshToken\": \"rt\",\n  \"clientId\": \"c\"\n}\n  ]\n}";
        let v = parse_import_json(raw).expect("尾随括号应被容忍");
        assert_eq!(v.get("refreshToken").and_then(|x| x.as_str()), Some("rt"));
        let accts = parse_accounts_export(&v).unwrap();
        assert_eq!(accts.len(), 1);
        assert_eq!(accts[0].extra["refresh_token"], json!("rt"));
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

    // ───────── 官方 API Key(ksk_)导入 ─────────

    #[test]
    fn bare_api_key_string_maps_to_apikey_account() {
        let v = json!(["ksk_TESTONLY_NOT_A_REAL_KEY_00000000"]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.extra["kiro_api_key"], json!("ksk_TESTONLY_NOT_A_REAL_KEY_00000000"));
        assert_eq!(a.extra["auth_method"], json!("api_key"));
        // 不镜像 access_token(bearer 直取 kiro_api_key);无 refresh_token / profile_arn。
        assert!(!a.extra.contains_key("access_token"));
        assert!(!a.extra.contains_key("refresh_token"));
        assert!(!a.extra.contains_key("profile_arn"));
        // account_id 由整 key 的 sha256 前 12 位派生(抗碰撞、不泄露 key)。
        assert!(a.account_id.starts_with("kiro-apikey-"));
        assert_eq!(a.account_id.len(), "kiro-apikey-".len() + 12);
        assert!(!a.account_id.contains("MhyDXDHU"), "不得含 key 明文尾");
    }

    #[test]
    fn non_ksk_input_is_rejected() {
        // 裸串路径:非 ksk_ 前缀不建号(防任意粘贴串变账号)。
        assert!(map_api_key("hello world", None).is_none());
        assert!(map_api_key("sk-1234567890", None).is_none());
        assert!(map_api_key("ksk_", None).is_none()); // 仅前缀、无实体
        assert!(map_api_key("ksk_x", None).is_some());
        // Unicode 输入不 panic(旧尾 8 字节切法会崩;现走 sha256)。
        assert!(map_api_key("中文非法键", None).is_none());
        // parse_accounts_export 混入非法裸串:被跳过,不报错。
        let v = json!(["ksk_good1234", "garbage", "中文"]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].extra["kiro_api_key"], json!("ksk_good1234"));
    }

    #[test]
    fn split_api_keys_filters_non_ksk() {
        let raw = "ksk_aaa\nnot-a-key\nsk-xyz\nksk_bbb";
        assert_eq!(split_api_keys(raw), vec!["ksk_aaa", "ksk_bbb"]);
    }

    #[test]
    fn flat_object_with_kiro_api_key_maps_apikey() {
        let v = json!([{"kiro_api_key": "ksk_abc12345", "email": "u@x.com", "region": "us-east-1"}]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.extra["kiro_api_key"], json!("ksk_abc12345"));
        assert_eq!(a.extra["region"], json!("us-east-1"));
        // 带 email 时 account_id 用 email。
        assert_eq!(a.account_id, "u-x.com");
        assert!(!a.extra.contains_key("refresh_token"));
    }

    #[test]
    fn flat_camel_apiKey_field_recognized() {
        let v = json!([{"apiKey": "ksk_zzz99999"}]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out[0].extra["kiro_api_key"], json!("ksk_zzz99999"));
    }

    #[test]
    fn split_api_keys_splits_dedups_preserves_order() {
        let raw = "ksk_a\nksk_b , ksk_a\n\n  ksk_c  \n";
        assert_eq!(split_api_keys(raw), vec!["ksk_a", "ksk_b", "ksk_c"]);
        assert!(split_api_keys("   \n  ").is_empty());
    }

    #[test]
    fn mixed_refresh_and_apikey_entries_both_parse() {
        let v = json!([
            {"refresh_token": "rt-1"},
            "ksk_bare9999",
            {"kiro_api_key": "ksk_obj7777"},
        ]);
        let out = parse_accounts_export(&v).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].extra["refresh_token"], json!("rt-1"));
        assert_eq!(out[1].extra["kiro_api_key"], json!("ksk_bare9999"));
        assert_eq!(out[2].extra["kiro_api_key"], json!("ksk_obj7777"));
    }

    #[test]
    fn empty_api_key_string_is_skipped() {
        assert!(map_api_key("   ", None).is_none());
    }
}
