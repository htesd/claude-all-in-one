//! Kiro 上游请求头构造 —— 逐字节对齐 static_flow(最新生产客户端形态)。
//!
//! 金标准来源:`_refs/static_flow/crates/llm-access/src/kiro_headers.rs` +
//! `provider/kiro_protocol.rs` + `kiro_refresh.rs`(commit 9051d71)。封号根因点在
//! UA 里嵌的 machineId 与端点指纹,这里集中管理,改动需对照 static_flow。
//!
//! 主推理 `generateAssistantResponse`:
//! - URL `https://runtime.{region}.kiro.dev/generateAssistantResponse`
//!   (env `KIRO_RUNTIME_UPSTREAM_BASE_URL` / `KIRO_UPSTREAM_BASE_URL` 可覆盖)
//! - UA `aws-sdk-js/1.0.34 ua/2.1 os/darwin#24.6.0 lang/js md/nodejs#22.22.0
//!   api/codewhispererstreaming#1.0.34 KiroIDE-{ver}-{machine}`(**无 `m/E`**)
//! - 条件头:`TokenType: EXTERNAL_IDP`(auth_method=external_idp)、
//!   `redirect-for-internal: true`(provider=internal)。

use gw_core::account::Account;

/// AWS SDK 版本(主推理流)。对齐 static_flow `KIRO_PROVIDER_AWS_SDK_VERSION`。
pub(crate) const AWS_SDK_VERSION: &str = "1.0.34";
/// 默认 Kiro 客户端版本(可被 account.extra["kiro_version"] 覆盖)。
pub(crate) const DEFAULT_KIRO_VERSION: &str = "0.12.155";
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

/// 主推理上游 base url:env 覆盖优先(对齐 static_flow `configured_upstream_base_url`),
/// 否则默认 `https://runtime.{region}.kiro.dev`。
pub(crate) fn runtime_base_url(region: &str) -> String {
    let env_override = read_base_env(RUNTIME_BASE_ENV).or_else(|| read_base_env(UPSTREAM_BASE_ENV));
    runtime_base_url_from(region, env_override)
}

/// 纯逻辑(env 注入便于测试):覆盖值已 trim 去尾斜杠。
fn runtime_base_url_from(region: &str, env_override: Option<String>) -> String {
    env_override.unwrap_or_else(|| format!("https://runtime.{region}.kiro.dev"))
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

/// 账号的有效 Kiro 客户端版本(extra 覆盖 > 默认)。
pub(crate) fn kiro_version(account: &Account) -> String {
    account
        .extra_str("kiro_version")
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_KIRO_VERSION)
        .to_string()
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
    if is_external_idp(account) {
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
pub(crate) fn resolve_profile_arn(account: &Account) -> Option<String> {
    account
        .extra_str("profile_arn")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| fixed_profile_arn(account).map(|s| s.to_string()))
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
        assert_eq!(
            runtime_base_url_from("us-east-1", None),
            "https://runtime.us-east-1.kiro.dev"
        );
        assert_eq!(
            runtime_base_url_from("eu-central-1", None),
            "https://runtime.eu-central-1.kiro.dev"
        );
    }

    #[test]
    fn runtime_base_url_honors_env_override() {
        assert_eq!(
            runtime_base_url_from("us-east-1", Some("https://q.us-east-1.amazonaws.com".into())),
            "https://q.us-east-1.amazonaws.com"
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
}
