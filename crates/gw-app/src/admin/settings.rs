//! admin 系统设置端点 —— 热调参数面板的后端。
//!
//! 设置模型:`system.yaml`([`SystemConfig`])是不可变基线(改它需重启),DB `settings`
//! 表存**字段级 overlay**([`SystemSettings`],仅非 None 字段覆盖)。worker 30s 轮询 DB
//! 后热应用,无需重启。
//!
//! - `GET /settings`:返回**有效全量**(YAML 基线叠 DB overlay,每字段都有值),前端据此
//!   渲染当前生效值。
//! - `PUT /settings`:吃**部分** patch,`null` 字段 = 删该 overlay 回 YAML 默认、非 null =
//!   存 overlay。返回叠加后的新有效全量。
//!
//! 进程拓扑(端口/每 worker 源 IP)**不在此**(留 instances.yaml,需重启)。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use gw_core::config::{SystemConfig, SystemSettings};

use super::{internal_error, AdminState};

pub fn router() -> Router<AdminState> {
    Router::new().route("/settings", get(get_settings).put(put_settings))
}

/// 4xx 错误响应(请求体问题)。
fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({"type":"error","error":{"message": msg}}))).into_response()
}

/// 由 DB overlay JSON 解析出 [`SystemSettings`](解析失败/无行 → 默认空 overlay)。
fn read_overlay(st: &AdminState) -> anyhow::Result<SystemSettings> {
    match st.store.get_settings()? {
        Some(j) => Ok(serde_json::from_str(&j).unwrap_or_default()),
        None => Ok(SystemSettings::default()),
    }
}

/// 把 overlay 叠到 YAML 基线得"有效"配置,再回灌成全量 [`SystemSettings`](含 default_proxy)。
fn effective(st: &AdminState, overlay: &SystemSettings) -> SystemSettings {
    let mut cfg: SystemConfig = (*st.yaml_config).clone();
    overlay.apply_to(&mut cfg);
    let mut full = SystemSettings::from_effective(&cfg, overlay.default_proxy.clone());
    // egress_pool 与 default_proxy 同属「不进 SystemConfig」的出口字段,from_effective 不含它,
    // 这里从 overlay 原样回灌(供前端展示当前池)。
    full.egress_pool = overlay.egress_pool.clone();
    full
}

/// 同 [`respond`],额外挂一个 `workers` 字段(逐 worker 实然值)。
///
/// 单开一个函数而不是「先 respond 再把 body 拆出来改」:后者要走
/// 序列化→解析→再序列化的往返,还要操心 Content-Length 与「body 不是 object 就 panic」
/// 这类边角(对抗审查 Skeptic#8)。多传一个参数便宜得多。
fn respond_with_workers(
    settings: SystemSettings,
    workers: Vec<serde_json::Value>,
) -> axum::response::Response {
    let mut v = redacted_value(&settings);
    if let serde_json::Value::Object(map) = &mut v {
        map.insert("workers".into(), serde_json::Value::Array(workers));
    }
    Json(v).into_response()
}

/// 把有效设置序列化为响应,并掩码 default_proxy 的密码段(防 user:pass@ 经接口泄漏,
/// 审查 Architect#3)。真实值仍在库里,worker 用真实值。
fn respond(settings: SystemSettings) -> axum::response::Response {
    Json(redacted_value(&settings)).into_response()
}

/// 掩码后的 JSON 值(两个 respond 共用,免得掩码规则分叉)。
fn redacted_value(settings: &SystemSettings) -> serde_json::Value {
    let mut v = serde_json::to_value(settings).unwrap_or(serde_json::Value::Null);
    if let Some(dp) = v.get("default_proxy").and_then(|d| d.as_str()) {
        let masked = super::redact_proxy_url(dp);
        v["default_proxy"] = serde_json::json!(masked);
    }
    // egress_pool 每条 URL 同样可含 user:pass@,逐条掩码密码段(真实值仍在库里)。
    if let Some(arr) = v.get("egress_pool").and_then(|d| d.as_array()) {
        let masked: Vec<serde_json::Value> = arr
            .iter()
            .map(|item| match item.as_str() {
                Some(s) => serde_json::json!(super::redact_proxy_url(s)),
                None => item.clone(),
            })
            .collect();
        v["egress_pool"] = serde_json::Value::Array(masked);
    }
    v
}

/// `GET /settings` → 当前有效全量设置。
/// 逐 worker 问「你此刻**真正在用**的热调值是什么」。
///
/// 为什么 GET /settings 要带上它:这个接口回的是「库里存的 + YAML 基线」算出来的**应然**值,
/// 而 worker 用的是它自己 30s 轮询应用后的**实然**值。两者不一致正是「我保存了不生效」的
/// 全部内容,而在此之前面板上只有应然值 —— 于是那个问题在面板上根本不可见。
///
/// 单个 worker 离线/超时不影响整体:该条标 `online:false`,其余照常回。
async fn worker_settings(st: &AdminState) -> Vec<serde_json::Value> {
    let fetches = st.workers.iter().map(|w| {
        let http = st.http.clone();
        async move {
            // 打**轻量**端点而不是 /health:后者会跑全账号快照并对配额缓存陈旧的账号
            // 触发上游 getUsageLimits —— 「打开设置页」不该变成「对付费账号打上游」。
            //
            // 超时靠 `st.http` 的全局 2s(见 `AdminState` 构造),各 worker 并发,
            // 所以一个挂住的 worker 最多让本接口慢 2 秒,不会拖死设置页。
            let url = format!("http://{}/settings-sync", w.listen);
            let fetched = match http.get(&url).send().await {
                // 状态码必须看:非 2xx 也可能带一个能解析成 JSON 的错误体,当成正常
                // 回包会把坏掉的 worker 显示成健康(对抗审查 Skeptic#12)。
                Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
                    Ok(v) => Fetched::Body(v),
                    Err(_) => Fetched::NoData,
                },
                // **答得出话就说明进程活着**,只是这个路由/字段它没有(旧镜像 404)。
                Ok(_) => Fetched::NoData,
                Err(_) => Fetched::Unreachable,
            };
            worker_row(w.instance, &w.account_group, fetched)
        }
    });
    futures::future::join_all(fetches).await
}

/// 一次抓取的三种结局。
///
/// 分成三态而不是 `Option`,是因为后两种指向**完全不同的运维动作**:
/// 连不上 = 进程可能死了,该去看容器;连得上但没这个路由 = 进程活得好好的,是镜像旧。
/// 把后者显示成「离线」会把人指向错误的方向 —— 2026-08-06 上线时实测撞到:
/// `caio-worker-dario` 跑着旧镜像,`/settings-sync` 返回 404,一度被判成掉线。
enum Fetched {
    /// 连不上(拒绝连接 / 超时)。
    Unreachable,
    /// 答得出话,但拿不到可用的 settings(旧镜像 404、非 2xx、body 解析失败)。
    NoData,
    Body(serde_json::Value),
}

/// 把一次抓取结果映射成面板要的一行。**纯函数**,把扇出里唯一有判断的部分抽出来测 ——
/// 这几个分支正是本功能最想抓的场景,不该只靠线上观察(对抗审查 Skeptic#7)。
fn worker_row(instance: u32, group: &str, fetched: Fetched) -> serde_json::Value {
    let mut o = serde_json::json!({
        "instance": instance, "group": group, "online": false,
        "settings": serde_json::Value::Null, "stale_image": false,
    });
    let body = match fetched {
        Fetched::Unreachable => return o,
        // ⚠️ 这条是本次改动**最想抓**的那个场景:worker 镜像旧到还没有这个端点/字段。
        //
        // 若把它和「正常」一样留成 null,前端拿到 null 会走「无差异」分支渲染成绿色
        // 「一致」—— 可观测性在它唯一存在的理由上说谎(对抗审查 Skeptic#1)。
        Fetched::NoData => {
            o["online"] = true.into();
            o["stale_image"] = true.into();
            return o;
        }
        Fetched::Body(v) => v,
    };
    o["online"] = true.into();
    match body.get("settings") {
        Some(s) if !s.is_null() => o["settings"] = s.clone(),
        _ => o["stale_image"] = true.into(),
    }
    o
}

async fn get_settings(State(st): State<AdminState>) -> axum::response::Response {
    let overlay = match read_overlay(&st) {
        Ok(o) => o,
        Err(e) => return internal_error(e),
    };
    // 把逐 worker 的实然值挂在同一份响应里:面板在**改设置的那一页**就能核对,
    // 不用切页、更不用 SSH 上去查库。
    //
    // `PUT` 走不带 workers 的 `respond`:刚写完库时 worker 还没轮询到,
    // 回一份必然过时的实然值只会让人以为没生效。
    respond_with_workers(effective(&st, &overlay), worker_settings(&st).await)
}

/// `PUT /settings` ← 部分 patch(null=删该 overlay 字段回 YAML 默认,非 null=存)。
/// 返回叠加后的新有效全量。
async fn put_settings(
    State(st): State<AdminState>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let serde_json::Value::Object(patch) = body else {
        return api_error(StatusCode::BAD_REQUEST, "请求体应为 JSON 对象");
    };

    // 现有 overlay(原始 JSON map)→ 合并 patch:null 删键、非 null 设键。
    let mut overlay_map: serde_json::Map<String, serde_json::Value> = match st.store.get_settings() {
        Ok(Some(j)) => serde_json::from_str(&j).unwrap_or_default(),
        Ok(None) => serde_json::Map::new(),
        Err(e) => return internal_error(e),
    };
    for (k, v) in patch {
        if v.is_null() {
            overlay_map.remove(&k);
            continue;
        }
        // default_proxy:空串=清除;非空=写入边界校验 + 归一(fail-closed,审查 Skeptic#2)。
        if k == "default_proxy" {
            match v.as_str() {
                Some(s) if s.trim().is_empty() => {
                    overlay_map.remove(&k);
                }
                Some(s) => match super::validate_proxy_url(s) {
                    Ok(valid) => {
                        overlay_map.insert(k, serde_json::json!(valid));
                    }
                    Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
                },
                None => return api_error(StatusCode::BAD_REQUEST, "default_proxy 须为字符串"),
            }
            continue;
        }
        // egress_pool:出口代理池(数组)。空数组=清除;非空=逐条 validate_proxy_url(fail-closed,
        // 同 default_proxy:拒绝含 *** 掩码的回传值,绝不把脱敏形态当真值存)。
        if k == "egress_pool" {
            match v.as_array() {
                Some(arr) if arr.is_empty() => {
                    overlay_map.remove(&k);
                }
                Some(arr) => {
                    let mut validated = Vec::with_capacity(arr.len());
                    for item in arr {
                        let Some(s) = item.as_str() else {
                            return api_error(StatusCode::BAD_REQUEST, "egress_pool 每项须为字符串");
                        };
                        match super::validate_proxy_url(s) {
                            Ok(valid) => validated.push(serde_json::Value::String(valid)),
                            Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
                        }
                    }
                    overlay_map.insert(k, serde_json::Value::Array(validated));
                }
                None => return api_error(StatusCode::BAD_REQUEST, "egress_pool 须为数组"),
            }
            continue;
        }
        // default_thinking_effort:必须命中上游档位全集。这个值会**原样进 wire**
        // (`additionalModelRequestFields.effort`),脏值换来的是上游 400 —— 而且要等到下一次
        // 真实请求才暴露,所以在写库前就挡住。空串=清除回 YAML 默认。
        if k == "default_thinking_effort" {
            match v.as_str() {
                Some(s) if s.trim().is_empty() => {
                    overlay_map.remove(&k);
                }
                Some(s) => match gw_kiro::anthropic_types::normalize_effort(Some(s)) {
                    // 归一后存标准小写形态(上游只认小写),而不是原样存用户输入。
                    (canonical, false) => {
                        overlay_map.insert(k, serde_json::json!(canonical));
                    }
                    (_, true) => {
                        return api_error(
                            StatusCode::BAD_REQUEST,
                            &format!(
                                "default_thinking_effort 不是合法档位: {s};可选 {}",
                                gw_kiro::anthropic_types::VALID_EFFORTS.join(" / ")
                            ),
                        )
                    }
                },
                None => {
                    return api_error(StatusCode::BAD_REQUEST, "default_thinking_effort 须为字符串")
                }
            }
            continue;
        }
        overlay_map.insert(k, v);
    }

    // 校验合并后的 overlay 能解析为 SystemSettings(类型不符直接拒,不写坏库)。
    let overlay: SystemSettings = match serde_json::from_value(serde_json::Value::Object(
        overlay_map.clone(),
    )) {
        Ok(s) => s,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("设置字段不合法: {e}")),
    };
    // 拼错 key 的保护。`SystemSettings` 的 `deny_unknown_fields` 已于 2026-07-31 移除
    // (它同时卡死了 worker 读侧的容错,一个陌生 key 就让整份 overlay 归零 → 全量 503,
    // 见该结构体的文档),保护改由**写侧**承担:`unknown` 装的就是没被任何字段认领的 key。
    //
    // ⚠️ 这道闸是 overlay 里**唯一**的拼错防线 —— 读侧现在只告警不拒绝。
    if !overlay.unknown.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            &format!("未知设置字段: {}", overlay.unknown_keys().join(", ")),
        );
    }

    let json = serde_json::to_string(&overlay_map).unwrap_or_else(|_| "{}".into());
    if let Err(e) = st.store.upsert_settings(&json) {
        return internal_error(e);
    }
    respond(effective(&st, &overlay))
}

#[cfg(test)]
mod tests {
    use super::super::tests_support::{app, req};
    use super::{worker_row, Fetched};
    use axum::body::to_bytes;
    use tower::ServiceExt;

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn get_returns_yaml_defaults_when_no_overlay() {
        let (app, _store) = app();
        let resp = app.oneshot(req("GET", "/settings", None)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        // 默认 SystemConfig:max_failures=5、cache_read_multiplier=1.0、无 default_proxy。
        assert_eq!(v["max_failures"], 5);
        assert_eq!(v["cache_read_multiplier"], 1.0);
        assert!(v.get("default_proxy").map(|d| d.is_null()).unwrap_or(true));
    }

    #[test]
    fn 抓不到或没有settings字段的worker绝不能被当成一致() {
        // 连不上 → online=false,没有任何「一致」可言。
        let off = worker_row(0, "G0", Fetched::Unreachable);
        assert_eq!(off["online"], false);
        assert_eq!(off["stale_image"], false);
        assert!(off["settings"].is_null());

        // **答得出话就说明进程活着**,只是没这个路由(旧镜像 404)。
        // 报成「离线」会让人去查容器是不是死了,而真正该做的是重建镜像 ——
        // 2026-08-06 上线时 caio-worker-dario 正是这种,一度被判成掉线。
        let stale = worker_row(1, "DARIO", Fetched::NoData);
        assert_eq!(stale["online"], true, "404 不等于进程死了");
        assert_eq!(stale["stale_image"], true);

        // 2xx 但回包里没有 settings 字段,同样是旧镜像。
        // 漏标的话前端会拿 null 走「无差异」分支渲染绿勾 —— 恰好在本功能唯一存在的理由上说谎。
        let nofield = worker_row(2, "G0", Fetched::Body(serde_json::json!({"role":"worker"})));
        assert_eq!(nofield["online"], true);
        assert_eq!(nofield["stale_image"], true, "旧镜像必须单独标出,不能混进「一致」");

        // settings 显式为 null 同样算旧镜像,不能当成「有数据」。
        let nulled = worker_row(3, "G0", Fetched::Body(serde_json::json!({"settings": null})));
        assert_eq!(nulled["stale_image"], true);

        // 正常回包才透传。
        let ok = worker_row(
            4,
            "G0",
            Fetched::Body(serde_json::json!({"settings": {"applied_at": 42, "provider_hot": true}})),
        );
        assert_eq!(ok["online"], true);
        assert_eq!(ok["stale_image"], false);
        assert_eq!(ok["settings"]["applied_at"], 42);
    }

    #[tokio::test]
    async fn get_带上逐worker的实然值而put不带() {
        // 这是「保存了不生效」唯一能被看见的地方:GET 回的是应然值(库+YAML),
        // `workers` 回的是各 worker 真正在用的值。两者必须在**同一份响应**里,
        // 否则面板要么显示不出差异、要么得再开一个接口。
        let (app, _store) = app();
        let resp = app.clone().oneshot(req("GET", "/settings", None)).await.unwrap();
        let v = body_json(resp).await;
        assert!(v["workers"].is_array(), "GET 必须带 workers(没有 worker 时是空数组)");

        // PUT **不带**:刚写完库时 worker 还没轮询到,回一份必然过时的实然值
        // 只会让人误判成「没生效」。
        let resp = app
            .oneshot(req("PUT", "/settings", Some(r#"{"cache_floor_ratio":0.5}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["cache_floor_ratio"], 0.5);
        assert!(v.get("workers").is_none(), "PUT 不该回 workers");
    }

    #[tokio::test]
    async fn put_stores_overlay_and_get_reflects_it() {
        let (app, _store) = app();
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/settings",
                Some(r#"{"rate_limit_cooldown_secs": 600, "default_proxy": "socks5://h:1080"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["rate_limit_cooldown_secs"], 600);
        assert_eq!(v["default_proxy"], "socks5://h:1080");
        // 再 GET 应持久反映。
        let resp2 = app.oneshot(req("GET", "/settings", None)).await.unwrap();
        let v2 = body_json(resp2).await;
        assert_eq!(v2["rate_limit_cooldown_secs"], 600);
        assert_eq!(v2["default_proxy"], "socks5://h:1080");
    }

    #[tokio::test]
    async fn get_exposes_default_thinking_effort() {
        // 前端要靠它渲染当前生效档位;字段一旦漏了,下拉框就没有初值。
        let (app, _store) = app();
        let v = body_json(app.oneshot(req("GET", "/settings", None)).await.unwrap()).await;
        assert_eq!(
            v["default_thinking_effort"],
            gw_core::config::DEFAULT_THINKING_EFFORT.as_str()
        );
    }

    #[tokio::test]
    async fn put_default_thinking_effort_stores_canonical_lowercase() {
        let (app, _store) = app();
        let v = body_json(
            app.clone()
                .oneshot(req("PUT", "/settings", Some(r#"{"default_thinking_effort":"XHigh"}"#)))
                .await
                .unwrap(),
        )
        .await;
        // 归一成小写后存 —— 上游只认小写形态,不能原样把用户输入透过去。
        assert_eq!(v["default_thinking_effort"], "xhigh");
        let v2 = body_json(app.oneshot(req("GET", "/settings", None)).await.unwrap()).await;
        assert_eq!(v2["default_thinking_effort"], "xhigh", "应持久");
    }

    #[tokio::test]
    async fn put_rejects_illegal_thinking_effort() {
        // 脏档位会原样进 wire 换来上游 400,且要等下一次真实请求才暴露 —— 必须写库前就挡。
        let (app, _store) = app();
        let resp = app
            .clone()
            .oneshot(req("PUT", "/settings", Some(r#"{"default_thinking_effort":"ludicrous"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let v = body_json(resp).await;
        let msg = v["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("ludicrous"), "错误里要点名非法值,实际={msg}");
        assert!(msg.contains("xhigh"), "错误里要列出可选档位,实际={msg}");
        // 关键:被拒后库里不该留下任何痕迹,仍是基线默认。
        let v2 = body_json(app.oneshot(req("GET", "/settings", None)).await.unwrap()).await;
        assert_eq!(
            v2["default_thinking_effort"],
            gw_core::config::DEFAULT_THINKING_EFFORT.as_str()
        );
    }

    #[tokio::test]
    async fn put_empty_thinking_effort_resets_to_baseline() {
        let (app, _store) = app();
        app.clone()
            .oneshot(req("PUT", "/settings", Some(r#"{"default_thinking_effort":"low"}"#)))
            .await
            .unwrap();
        // 空串 = 清 overlay 回 YAML 基线(与 default_proxy 同口径)。
        let v = body_json(
            app.clone()
                .oneshot(req("PUT", "/settings", Some(r#"{"default_thinking_effort":"  "}"#)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            v["default_thinking_effort"],
            gw_core::config::DEFAULT_THINKING_EFFORT.as_str()
        );
    }

    #[tokio::test]
    async fn put_null_resets_field_to_yaml_default() {
        let (app, _store) = app();
        // 先设 overlay。
        app.clone()
            .oneshot(req("PUT", "/settings", Some(r#"{"max_failures": 1}"#)))
            .await
            .unwrap();
        // null 重置回 YAML 默认(5)。
        let resp = app
            .clone()
            .oneshot(req("PUT", "/settings", Some(r#"{"max_failures": null}"#)))
            .await
            .unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["max_failures"], 5, "null 应回退到 YAML 默认");
    }

    #[tokio::test]
    async fn put_rejects_wrong_type() {
        let (app, _store) = app();
        let resp = app
            .oneshot(req("PUT", "/settings", Some(r#"{"max_failures": "not-a-number"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "类型不合法应 400,不写坏库");
    }

    #[tokio::test]
    async fn put_rejects_unknown_field() {
        let (app, _store) = app();
        // 拼错 key(max_failure 少了 s)应 400,不静默落库死 overlay。
        // 机制已于 2026-07-31 从 `deny_unknown_fields` 换成「`SystemSettings::unknown` 非空即拒」
        // —— 前者同时卡死了 worker 读侧的容错(一个陌生 key 作废整份 overlay → 全量 503)。
        let resp = app
            .oneshot(req("PUT", "/settings", Some(r#"{"max_failure": 1}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "未知字段应 400");
    }

    #[tokio::test]
    async fn put_unknown_field_error_names_the_key() {
        let (app, _store) = app();
        let resp = app
            .oneshot(req("PUT", "/settings", Some(r#"{"max_failure": 1}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.contains("max_failure"),
            "报错要点名是哪个 key 拼错了,否则运维得自己猜:{s}"
        );
    }

    #[tokio::test]
    async fn put_rejects_invalid_default_proxy() {
        let (app, _store) = app();
        // 含掩码占位的回传值必须拒绝(fail-closed,绝不把脱敏形态当真值存)。
        let resp = app
            .oneshot(req("PUT", "/settings", Some(r#"{"default_proxy": "socks5://u:***@h:1080"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "非法/掩码代理应 400");
    }

    #[tokio::test]
    async fn put_default_proxy_password_redacted_in_response() {
        let (app, _store) = app();
        let resp = app
            .oneshot(req(
                "PUT",
                "/settings",
                Some(r#"{"default_proxy": "socks5://user:pass@host:1080"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(
            v["default_proxy"], "socks5://user:***@host:1080",
            "响应里默认代理的密码段必须掩码"
        );
    }

    #[tokio::test]
    async fn settings_requires_auth() {
        let (app, _store) = app();
        let unauth = axum::http::Request::builder()
            .method("GET")
            .uri("/settings")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(unauth).await.unwrap();
        assert_eq!(resp.status(), 401);
    }
}
