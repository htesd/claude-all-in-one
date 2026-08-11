//! Kiro 上游请求头构造 —— 逐字节对齐 static_flow(最新生产客户端形态)。
//!
//! 金标准来源:`_refs/static_flow/crates/llm-access/src/kiro_headers.rs` +
//! `provider/kiro_protocol.rs` + `kiro_refresh.rs`(commit 9051d71)。封号根因点在
//! UA 里嵌的 machineId 与端点指纹,这里集中管理,改动需对照 static_flow。
//!
//! 主推理 `generateAssistantResponse`:
//! - URL `https://runtime.{region}.kiro.dev/generateAssistantResponse`(默认/现状)
//!   或 `https://q.{region}.amazonaws.com/generateAssistantResponse`(`q_endpoint` 开关开,
//!   与 kiro.rs 一致、做服务端 prompt 缓存);env `KIRO_RUNTIME_UPSTREAM_BASE_URL` /
//!   `KIRO_UPSTREAM_BASE_URL` 整串覆盖优先(见 [`runtime_base_url`])
//! - UA `aws-sdk-js/1.0.34 ua/2.1 os/darwin#24.6.0 lang/js md/nodejs#22.22.0
//!   api/codewhispererstreaming#1.0.34 KiroIDE-{ver}-{machine}`(**无 `m/E`**)
//! - 条件头:`TokenType: EXTERNAL_IDP`(auth_method=external_idp)、
//!   `redirect-for-internal: true`(provider=internal)。

use gw_core::account::Account;

/// AWS SDK 版本(主推理流)。对齐 static_flow `KIRO_PROVIDER_AWS_SDK_VERSION`。
pub(crate) const AWS_SDK_VERSION: &str = "1.0.34";
/// 默认 Kiro 客户端版本(可被 account.extra["kiro_version"] 覆盖)。
///
/// 2026-07-28 对齐真实客户端:deb `kiro 1.0.212-1784842874`,`product.json` version=1.0.212
/// (commit 8848ae36,build 2026-07-23)。UA 拼法未变,仍是
/// `KiroIDE-${kiroVersion}-${machineId}`(`extension.js:374150`)。
pub(crate) const CURRENT_KIRO_VERSION: &str = "1.0.212";
/// 2026-07-28 之前的旧默认版本。`KIRO_LEGACY_WIRE=1`(见 wire_profile)时 UA 回到它,
/// 与旧报文形态配套 —— 版本号和报文形态必须同属一个时代,混搭是最差形态。
pub(crate) const LEGACY_KIRO_VERSION: &str = "0.12.155";
/// UA 里的系统/Node 版本,逐字对齐 static_flow `DEFAULT_SYSTEM_VERSION`/`DEFAULT_NODE_VERSION`。
pub(crate) const DEFAULT_SYSTEM_VERSION: &str = "darwin#24.6.0";
pub(crate) const DEFAULT_NODE_VERSION: &str = "22.22.0";

/// IdC(AWS SSO OIDC)刷新用的 AWS SDK 版本。对齐 static_flow `KIRO_IDC_AWS_SDK_VERSION`。
pub(crate) const IDC_AWS_SDK_VERSION: &str = "3.980.0";

/// Social(GitHub/Google)免费层共享 profileArn。对齐 static_flow `KIRO_SOCIAL_SIGN_IN_PROFILE_ARN`。
pub(crate) const SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
/// BuilderId 免费层共享 profileArn。对齐 static_flow `KIRO_BUILDER_ID_PROFILE_ARN`。
pub(crate) const BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

const RUNTIME_BASE_ENV: &str = "KIRO_RUNTIME_UPSTREAM_BASE_URL";
const UPSTREAM_BASE_ENV: &str = "KIRO_UPSTREAM_BASE_URL";

/// 主推理上游 base url。优先级:
/// 1. env 覆盖(`KIRO_RUNTIME_UPSTREAM_BASE_URL` / `KIRO_UPSTREAM_BASE_URL`,整串含 region,
///    对齐 static_flow `configured_upstream_base_url`)——最高优先,给显式全 URL 场景留后门;
/// 2. `q_endpoint` 开关(设置面板/env `KIRO_Q_ENDPOINT`):开 → `https://q.{region}.amazonaws.com`
///    (kiro.rs 端点,做服务端 prompt 缓存);
/// 3. 默认 → `https://runtime.{region}.kiro.dev`(现状,防封对齐当前 Kiro 客户端)。
pub(crate) fn runtime_base_url(region: &str) -> String {
    let env_override = read_base_env(RUNTIME_BASE_ENV).or_else(|| read_base_env(UPSTREAM_BASE_ENV));
    runtime_base_url_from(region, env_override, crate::converter::q_endpoint_enabled())
}

/// 纯逻辑(env 覆盖 + 端点开关注入便于测试)。env 覆盖已 trim 去尾斜杠;env 覆盖存在时
/// **无视** `q_endpoint`(显式全 URL 优先)。
fn runtime_base_url_from(region: &str, env_override: Option<String>, q_endpoint: bool) -> String {
    if let Some(base) = env_override {
        return base;
    }
    if q_endpoint {
        format!("https://q.{region}.amazonaws.com")
    } else {
        format!("https://runtime.{region}.kiro.dev")
    }
}

fn read_base_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
}

/// 从 base url 提 host 头(含端口)。对齐 static_flow `upstream_host_header`。
/// 解析失败时退回原串(调用方已保证是合法 URL)。
pub(crate) fn host_header(base_url: &str) -> String {
    match reqwest::Url::parse(base_url) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            (None, _) => base_url.to_string(),
        },
        Err(_) => base_url.to_string(),
    }
}

/// 主推理 UA 对(x-amz-user-agent, user-agent)。**主流 UA 无 `m/E`**(对齐 static_flow)。
pub(crate) fn streaming_user_agents(version: &str, machine_id: &str) -> (String, String) {
    let x_amz = format!("aws-sdk-js/{AWS_SDK_VERSION} KiroIDE-{version}-{machine_id}");
    let ua = format!(
        "aws-sdk-js/{AWS_SDK_VERSION} ua/2.1 os/{DEFAULT_SYSTEM_VERSION} lang/js \
         md/nodejs#{DEFAULT_NODE_VERSION} api/codewhispererstreaming#{AWS_SDK_VERSION} \
         KiroIDE-{version}-{machine_id}"
    );
    (x_amz, ua)
}

/// IdC 刷新 UA 对(x-amz-user-agent, user-agent),逐字对齐 static_flow:
/// - x-amz 带版本 `KiroIDE-{ver}`;
/// - user-agent **无** `api/sso-oidc`、**无**尾部 `KiroIDE`,带 `m/E`。
pub(crate) fn idc_refresh_user_agents(version: &str) -> (String, String) {
    let x_amz = format!("aws-sdk-js/{IDC_AWS_SDK_VERSION} KiroIDE-{version}");
    let ua = format!(
        "aws-sdk-js/{IDC_AWS_SDK_VERSION} ua/2.1 os/{DEFAULT_SYSTEM_VERSION} lang/js \
         md/nodejs#{DEFAULT_NODE_VERSION} m/E"
    );
    (x_amz, ua)
}

/// 账号的有效 Kiro 客户端版本(extra 覆盖 > 当前形态默认)。
///
/// ⚠️ **覆盖只改 UA 里自报的版本号,不会把报文形态一起改回去。** body 形态、
/// 配额端点等都跟着 `KIRO_LEGACY_WIRE` 总开关走(见 wire_profile)。把单个账号
/// 钉到与总开关不一致的版本,得到的是「自称 A 版本、行为却是 B 版本」的混搭 ——
/// 现实中不存在的组合,比不改更容易被识别。要回旧形态就用总开关整体回。
///
/// 保留这个字段是留一条将来支持多形态时的口子;在那之前,不等于生效默认版本就告警一次。
/// (2026-07-28 生产实测 222 个账号**无一**设置该字段,故此处只警示不改行为。)
pub(crate) fn kiro_version(account: &Account) -> String {
    let default = default_kiro_version();
    match account.extra_str("kiro_version").filter(|v| !v.is_empty()) {
        Some(v) if v != default => {
            warn_version_mismatch(v, default);
            v.to_string()
        }
        Some(v) => v.to_string(),
        None => default.to_string(),
    }
}

/// 当前生效的默认版本:legacy 总开关开 → 旧版本,否则现行版本。
pub(crate) fn default_kiro_version() -> &'static str {
    default_version_for(crate::wire_profile::legacy_wire())
}

/// 纯逻辑(开关注入便于测试;读 env 的测试会与并行用例互相污染)。
fn default_version_for(legacy: bool) -> &'static str {
    if legacy {
        LEGACY_KIRO_VERSION
    } else {
        CURRENT_KIRO_VERSION
    }
}

/// 每个偏离版本只吼一次,避免逐请求刷屏。
fn warn_version_mismatch(v: &str, wire_default: &'static str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut g = match seen.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(), // 告警路径不该因为一次 poison 就哑掉
    };
    if g.insert(v.to_string()) {
        tracing::warn!(
            account_kiro_version = v,
            wire_shape_version = wire_default,
            "账号钉了非默认 kiro_version：UA 自报该版本，但报文形态与端点仍是默认版本的，\
             这是现实中不存在的组合。除非确知在做什么，否则请清掉该字段。"
        );
    }
}

/// 把主推理金标准头加到请求上(顺序对齐 static_flow `add_kiro_headers`)。
/// `access_token` / `machine_id` / `version` 由调用方算好传入。
pub(crate) fn apply_streaming_headers(
    rb: reqwest::RequestBuilder,
    account: &Account,
    base_url: &str,
    access_token: &str,
    machine_id: &str,
    version: &str,
) -> reqwest::RequestBuilder {
    let (x_amz_ua, ua) = streaming_user_agents(version, machine_id);
    let mut rb = rb
        .header("content-type", "application/json")
        .header("accept", "application/vnd.amazon.eventstream")
        .header("x-amzn-kiro-agent-mode", "vibe")
        .header("x-amzn-codewhisperer-optout", "true");
    // 条件头(顺序对齐 static_flow:在 UA 之前)。
    // API Key 凭据:`TokenType: API_KEY` —— 官方 Kiro CLI headless 模式的真实客户端形态,
    // **服务端强制要求**(实测:缺此头则 ksk_ 被当普通 OAuth token,报 400「profileArn is
    // required」;带此头则免 profileArn、直接放行)。api_key 与 external_idp 互斥。
    if crate::machine_id::is_api_key_credential(account) {
        rb = rb.header("TokenType", "API_KEY");
    } else if is_external_idp(account) {
        rb = rb.header("TokenType", "EXTERNAL_IDP");
    }
    if is_internal_provider(account) {
        rb = rb.header("redirect-for-internal", "true");
    }
    rb.header("x-amz-user-agent", x_amz_ua)
        .header("user-agent", ua)
        .header("host", host_header(base_url))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .header("authorization", format!("Bearer {access_token}"))
}

/// 条件头判定:auth_method == "external_idp"(大小写不敏感)。
///
/// `pub(crate)`:token.rs 的刷新分流复用同一判定,避免两处各写一份 easily-diverging
/// 的字符串比较(此前只有本文件内部用,token.rs 的刷新分流当时压根没读 auth_method)。
pub(crate) fn is_external_idp(account: &Account) -> bool {
    account
        .extra_str("auth_method")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("external_idp"))
}

/// external_idp(Azure AD 企业 SSO)账号:给请求补 `TokenType: EXTERNAL_IDP` 头,否则
/// 保持原样。**所有** Kiro API 调用(不止 chat)对 external_idp 号都必须带此头——
/// CodeWhisperer 靠它识别令牌类型并按外部 IdP 校验 Azure JWT,缺了它会静默返回空
/// profile 列表并拒绝数据面调用(getUsageLimits/ListAvailableProfiles 得到 403)。
/// 对齐 Kiro-Go `applyKiroBaseHeaders`(其对每次 Kiro 请求统一注入)。streaming 头
/// 因需保持 static_flow 的严格字段顺序仍内联注入(见 `apply_streaming_headers`);
/// 顺序不敏感的辅助只读调用(配额/profile 发现)复用此助手,避免判定散落多处。
///
/// 位置:调用方在基础头(含 authorization)之后追加本头——与 Kiro-Go
/// `applyKiroBaseHeaders` 一致(那里 TokenType 也排在 Authorization/UA/optout 之后、
/// 位于末尾),故非"随手放末尾"的臆测,而是与参考实现的相对顺序对齐。这两条是 AWS
/// runtime REST 调用(getUsageLimits/ListAvailableProfiles),服务端按 header map 解析,
/// 不做 data-plane 那种逐字节指纹校验,顺序不影响识别。
pub(crate) fn apply_external_idp_token_type(
    rb: reqwest::RequestBuilder,
    account: &Account,
) -> reqwest::RequestBuilder {
    if is_external_idp(account) {
        rb.header("TokenType", "EXTERNAL_IDP")
    } else {
        rb
    }
}

/// Kiro 身份 provider(github/google/builderid/internal...)。注意:**不能**用
/// `Account.provider`(那是适配器家族 "kiro")或 extra["provider"](与顶层字段撞名,
/// JSON 里 `provider` 会被 serde flatten 吃到顶层而非 extra)——故用专用键 `kiro_provider`
/// (审查 Skeptic#2 / Architect#2)。
fn kiro_idp(account: &Account) -> Option<&str> {
    account.extra_str("kiro_provider").map(str::trim).filter(|v| !v.is_empty())
}

/// 条件头判定:Kiro idp == "internal"(大小写不敏感)。
fn is_internal_provider(account: &Account) -> bool {
    kiro_idp(account).is_some_and(|v| v.eq_ignore_ascii_case("internal"))
}

/// 缺失 profileArn 时的固定兜底(对齐 static_flow `fixed_profile_arn`):
/// github/google → social 共享 ARN;builderid/aws → builder 共享 ARN;其余(企业/内部/
/// external_idp)需动态 ListAvailableProfiles(未实现),返回 None 即省略 profileArn。
pub(crate) fn fixed_profile_arn(account: &Account) -> Option<&'static str> {
    let idp = kiro_idp(account)?;
    if idp.eq_ignore_ascii_case("github") || idp.eq_ignore_ascii_case("google") {
        Some(SOCIAL_PROFILE_ARN)
    } else if idp.eq_ignore_ascii_case("builderid")
        || idp.eq_ignore_ascii_case("builder-id")
        || idp.eq_ignore_ascii_case("aws")
    {
        Some(BUILDER_ID_PROFILE_ARN)
    } else {
        None
    }
}

/// 解析发包用的 profileArn:账号显式值优先,否则按 idp 取固定兜底。
///
/// API Key 凭据**永不带 profileArn**:`TokenType: API_KEY` 让服务端按 key 自身账号解析,
/// 反而带上(错误的)profileArn 会被拒。apikey 号无 kiro_provider,固定兜底本就返回 None,
/// 这里再显式短路一次作防御(万一误配了 profile_arn/kiro_provider)。
pub(crate) fn resolve_profile_arn(account: &Account) -> Option<String> {
    if crate::machine_id::is_api_key_credential(account) {
        return None;
    }
    account
        .extra_str("profile_arn")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| fixed_profile_arn(account).map(|s| s.to_string()))
}

/// 发包用的 bearer token(`Authorization: Bearer <token>` 的值)。
///
/// **凭据来源随类型分流**(单一事实来源,chat 与配额共用):
/// - API Key 账号 → `kiro_api_key`(ksk_,长期有效,由 `TokenType: API_KEY` 头激活);
/// - social / IdC 账号 → `access_token`(运行时刷新换取)。
///
/// 之所以不统一读 `access_token`:apikey 账号可能仅从 YAML/admin 建号、只带 `kiro_api_key`
/// 而无 `access_token`(不该强制用户镜像一份密钥),故按类型取真值。
pub(crate) fn bearer_token(account: &Account) -> Option<&str> {
    let field = if crate::machine_id::is_api_key_credential(account) {
        "kiro_api_key"
    } else {
        "access_token"
    };
    account.extra_str(field).filter(|s| !s.is_empty())
}

/// 把 IdC(AWS SSO OIDC)刷新金标准头加到请求上(头集合 + 值逐字对齐 static_flow
/// `refresh_idc`;含 `accept: */*`)。请求体由调用方 `.json(...)` 附加。
pub(crate) fn apply_idc_refresh_headers(
    rb: reqwest::RequestBuilder,
    region: &str,
    version: &str,
) -> reqwest::RequestBuilder {
    let (x_amz_ua, ua) = idc_refresh_user_agents(version);
    rb.header("content-type", "application/json")
        .header("host", format!("oidc.{region}.amazonaws.com"))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .header("connection", "close")
        .header("x-amz-user-agent", x_amz_ua)
        .header("accept", "*/*")
        .header("user-agent", ua)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn account_with(extra: serde_json::Value) -> Account {
        let mut acc = Account {
            account_id: "acc-1".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            extra: Default::default(),
        };
        if let serde_json::Value::Object(map) = extra {
            for (k, v) in map {
                acc.extra.insert(k, v);
            }
        }
        acc
    }

    #[test]
    fn runtime_base_url_defaults_to_kiro_dev() {
        // q_endpoint=false(默认)→ runtime.kiro.dev。
        assert_eq!(
            runtime_base_url_from("us-east-1", None, false),
            "https://runtime.us-east-1.kiro.dev"
        );
        assert_eq!(
            runtime_base_url_from("eu-central-1", None, false),
            "https://runtime.eu-central-1.kiro.dev"
        );
    }

    #[test]
    fn default_version_tracks_wire_profile() {
        // 版本号必须跟报文形态同属一个时代:legacy 开 → 0.12.155,关 → 1.0.212。
        assert_eq!(default_version_for(false), CURRENT_KIRO_VERSION);
        assert_eq!(default_version_for(true), LEGACY_KIRO_VERSION);
        assert_eq!(LEGACY_KIRO_VERSION, "0.12.155", "旧默认版本钉死,改动即改形态");
    }

    #[test]
    fn runtime_base_url_q_endpoint_switches_to_amazonaws() {
        // q_endpoint=true(无 env 覆盖)→ q.{region}.amazonaws.com(与 kiro.rs 一致)。
        assert_eq!(
            runtime_base_url_from("us-east-1", None, true),
            "https://q.us-east-1.amazonaws.com"
        );
        assert_eq!(
            runtime_base_url_from("eu-central-1", None, true),
            "https://q.eu-central-1.amazonaws.com"
        );
    }

    #[test]
    fn runtime_base_url_env_override_beats_q_endpoint() {
        // env 覆盖是整串,优先级最高——即便 q_endpoint=true 也用覆盖值(显式全 URL 后门)。
        let overridden = Some("https://custom.example.com".to_string());
        assert_eq!(
            runtime_base_url_from("us-east-1", overridden.clone(), true),
            "https://custom.example.com"
        );
        assert_eq!(
            runtime_base_url_from("us-east-1", overridden, false),
            "https://custom.example.com"
        );
    }

    #[test]
    fn host_header_strips_scheme_and_path() {
        assert_eq!(
            host_header("https://runtime.us-east-1.kiro.dev"),
            "runtime.us-east-1.kiro.dev"
        );
        assert_eq!(
            host_header("https://runtime.us-east-1.kiro.dev/generateAssistantResponse"),
            "runtime.us-east-1.kiro.dev"
        );
        assert_eq!(host_header("http://127.0.0.1:19090/x"), "127.0.0.1:19090");
    }

    #[test]
    fn streaming_ua_matches_static_flow_exactly() {
        let (x_amz, ua) = streaming_user_agents("0.12.155", "a".repeat(64).as_str());
        assert_eq!(
            x_amz,
            format!("aws-sdk-js/1.0.34 KiroIDE-0.12.155-{}", "a".repeat(64))
        );
        // 逐字对齐 static_flow kiro_user_agents(Streaming):无 m/E,os/node 带版本。
        assert_eq!(
            ua,
            format!(
                "aws-sdk-js/1.0.34 ua/2.1 os/darwin#24.6.0 lang/js md/nodejs#22.22.0 \
                 api/codewhispererstreaming#1.0.34 KiroIDE-0.12.155-{}",
                "a".repeat(64)
            )
        );
        assert!(!ua.contains("m/E"), "主推理 UA 不应含 m/E:{ua}");
    }

    #[test]
    fn streaming_headers_full_golden_set() {
        let acc = account_with(json!({}));
        let rb = reqwest::Client::new().post("https://runtime.us-east-1.kiro.dev/generateAssistantResponse");
        let rb = apply_streaming_headers(
            rb,
            &acc,
            "https://runtime.us-east-1.kiro.dev",
            "tok-123",
            &"b".repeat(64),
            "0.12.155",
        );
        let req = rb.build().unwrap();
        let h = req.headers();
        assert_eq!(h.get("content-type").unwrap(), "application/json");
        assert_eq!(h.get("accept").unwrap(), "application/vnd.amazon.eventstream");
        assert_eq!(h.get("x-amzn-kiro-agent-mode").unwrap(), "vibe");
        assert_eq!(h.get("x-amzn-codewhisperer-optout").unwrap(), "true");
        assert_eq!(h.get("host").unwrap(), "runtime.us-east-1.kiro.dev");
        assert_eq!(h.get("amz-sdk-request").unwrap(), "attempt=1; max=3");
        assert_eq!(h.get("authorization").unwrap(), "Bearer tok-123");
        assert!(h.get("amz-sdk-invocation-id").is_some());
        // 默认账号(非 external_idp、非 internal)不应带条件头。
        assert!(h.get("TokenType").is_none());
        assert!(h.get("redirect-for-internal").is_none());
    }

    #[test]
    fn idc_refresh_ua_matches_static_flow_exactly() {
        let (x_amz, ua) = idc_refresh_user_agents("0.12.155");
        assert_eq!(x_amz, "aws-sdk-js/3.980.0 KiroIDE-0.12.155");
        assert_eq!(
            ua,
            "aws-sdk-js/3.980.0 ua/2.1 os/darwin#24.6.0 lang/js md/nodejs#22.22.0 m/E"
        );
        assert!(!ua.contains("api/sso-oidc"), "IdC UA 不应含 api/sso-oidc:{ua}");
    }

    #[test]
    fn idc_refresh_headers_full_golden_set() {
        let rb = reqwest::Client::new().post("https://oidc.us-east-1.amazonaws.com/token");
        let req = apply_idc_refresh_headers(rb, "us-east-1", "0.12.155").build().unwrap();
        let h = req.headers();
        assert_eq!(h.get("content-type").unwrap(), "application/json");
        assert_eq!(h.get("host").unwrap(), "oidc.us-east-1.amazonaws.com");
        assert_eq!(h.get("amz-sdk-request").unwrap(), "attempt=1; max=4");
        assert_eq!(h.get("connection").unwrap(), "close");
        assert_eq!(h.get("x-amz-user-agent").unwrap(), "aws-sdk-js/3.980.0 KiroIDE-0.12.155");
        // static_flow IdC 刷新带 accept: */*(此前缺失,审查 Skeptic#3/Architect#3)。
        assert_eq!(h.get("accept").unwrap(), "*/*");
        assert!(h.get("amz-sdk-invocation-id").is_some());
        assert!(!h.get("user-agent").unwrap().to_str().unwrap().contains("api/sso-oidc"));
    }

    #[test]
    fn profile_arn_explicit_wins_then_fixed_fallback() {
        // 显式 profile_arn 优先。
        let acc = account_with(json!({"profile_arn": "arn:custom:x", "kiro_provider": "github"}));
        assert_eq!(resolve_profile_arn(&acc).as_deref(), Some("arn:custom:x"));
        // 缺失时按 idp 取固定 ARN(对齐 static_flow)。
        let gh = account_with(json!({"kiro_provider": "Github"}));
        assert_eq!(resolve_profile_arn(&gh).as_deref(), Some(SOCIAL_PROFILE_ARN));
        let bid = account_with(json!({"kiro_provider": "builder-id"}));
        assert_eq!(resolve_profile_arn(&bid).as_deref(), Some(BUILDER_ID_PROFILE_ARN));
        // idp 未知 / 缺失 → 省略(不臆造 ARN)。
        let unknown = account_with(json!({"kiro_provider": "enterprise"}));
        assert_eq!(resolve_profile_arn(&unknown), None);
        let none = account_with(json!({}));
        assert_eq!(resolve_profile_arn(&none), None);
    }

    #[test]
    fn internal_provider_reads_kiro_provider_not_top_level() {
        // 撞名修复:用 kiro_provider 而非 extra["provider"]。
        let acc = account_with(json!({"kiro_provider": "Internal"}));
        let rb = reqwest::Client::new().post("https://runtime.us-east-1.kiro.dev/x");
        let req = apply_streaming_headers(rb, &acc, "https://runtime.us-east-1.kiro.dev", "t", &"d".repeat(64), "0.12.155").build().unwrap();
        assert_eq!(req.headers().get("redirect-for-internal").unwrap(), "true");
    }

    #[test]
    fn external_idp_and_internal_conditional_headers() {
        let acc = account_with(json!({"auth_method": "external_idp", "kiro_provider": "Internal"}));
        let rb = reqwest::Client::new().post("https://runtime.us-east-1.kiro.dev/x");
        let rb = apply_streaming_headers(
            rb,
            &acc,
            "https://runtime.us-east-1.kiro.dev",
            "tok",
            &"c".repeat(64),
            "0.12.155",
        );
        let req = rb.build().unwrap();
        let h = req.headers();
        assert_eq!(h.get("TokenType").unwrap(), "EXTERNAL_IDP");
        assert_eq!(h.get("redirect-for-internal").unwrap(), "true");
    }

    #[test]
    fn apply_external_idp_token_type_adds_header_only_for_external_idp() {
        // external_idp 账号:补 TokenType 头(getUsageLimits/ListAvailableProfiles 都靠它)。
        // 大小写/写法不敏感(is_external_idp 用 eq_ignore_ascii_case):导入文件里可能是
        // "External_IDP"/"EXTERNAL_IDP" 等变体,都要命中,否则 Azure 号照样吃 403。
        for am in [
            json!({"auth_method": "external_idp"}),
            json!({"auth_method": "External_IDP"}),
            json!({"auth_method": "EXTERNAL_IDP"}),
            json!({"auth_method": "  external_idp  "}),
        ] {
            let acc = account_with(am.clone());
            let rb = reqwest::Client::new().get("https://q.us-east-1.amazonaws.com/x");
            let req = apply_external_idp_token_type(rb, &acc).build().unwrap();
            assert_eq!(req.headers().get("TokenType").unwrap(), "EXTERNAL_IDP", "{am} 应命中");
        }

        // 非 external_idp(social/idc/builderid):绝不加,免得误标令牌类型。
        for am in [json!({}), json!({"auth_method": "idc"}), json!({"auth_method": "social"})] {
            let acc = account_with(am);
            let rb = reqwest::Client::new().get("https://q.us-east-1.amazonaws.com/x");
            let req = apply_external_idp_token_type(rb, &acc).build().unwrap();
            assert!(req.headers().get("TokenType").is_none());
        }
    }

    #[test]
    fn api_key_account_sets_tokentype_api_key_header() {
        // 实测:runtime.kiro.dev 需要 `TokenType: API_KEY` 才免 profileArn。
        let acc = account_with(json!({"kiro_api_key": "ksk_abc", "auth_method": "api_key"}));
        let rb = reqwest::Client::new().post("https://runtime.us-east-1.kiro.dev/x");
        let req = apply_streaming_headers(
            rb,
            &acc,
            "https://runtime.us-east-1.kiro.dev",
            "ksk_abc", // bearer = 镜像的 ksk_
            &"a".repeat(64),
            "0.12.155",
        )
        .build()
        .unwrap();
        let h = req.headers();
        assert_eq!(h.get("TokenType").unwrap(), "API_KEY");
        assert_eq!(h.get("authorization").unwrap(), "Bearer ksk_abc");
    }

    #[test]
    fn api_key_account_never_carries_profile_arn() {
        // 即便误配了 profile_arn / kiro_provider,apikey 也必须省略 profileArn。
        let acc = account_with(json!({
            "kiro_api_key": "ksk_abc",
            "profile_arn": "arn:should:not:be:used",
            "kiro_provider": "github",
        }));
        assert_eq!(resolve_profile_arn(&acc), None);
    }

    #[test]
    fn bearer_token_source_by_credential_type() {
        // apikey:取 kiro_api_key(即便没有 access_token,如 YAML/admin 建号)。
        let apikey = account_with(json!({"kiro_api_key": "ksk_xyz"}));
        assert_eq!(bearer_token(&apikey), Some("ksk_xyz"));
        // social/IdC:取 access_token。
        let oauth = account_with(json!({"refresh_token": "rt", "access_token": "at-123"}));
        assert_eq!(bearer_token(&oauth), Some("at-123"));
        // 缺凭据 → None。
        assert_eq!(bearer_token(&account_with(json!({"refresh_token": "rt"}))), None);
    }
}
