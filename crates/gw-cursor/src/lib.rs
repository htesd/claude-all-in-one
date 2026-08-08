//! gw-cursor —— 把 Cursor 订阅(IDE 后端 ConnectRPC 协议)接成一个 Provider。
//!
//! 逆向自本机 Cursor **3.14.27**:对 `agentn.api5.cursor.sh` 的
//! `agent.v1.AgentService/Run` 发 HTTP/2 + ConnectRPC(protobuf,逐帧 gzip),
//! 鉴权 = `Bearer <session JWT>` + `x-cursor-checksum`(zyg cipher + machineId)。
//! 完整规格见 `PROTOCOL-agent-run.md`。
//!
//! 内部 IR 是 Anthropic Messages(与 gw-kiro/gw-dario 一致):`chat()` 吃 Anthropic
//! 请求体,吐 Anthropic SSE。
//!
//! ## 两个域名,别搞混
//!
//! - `agentn.api5.cursor.sh` —— 推理(`AgentService/Run`),BiDi 流。
//! - `api2.cursor.sh` —— unary 服务:`ServerConfigService/GetServerConfig`(取
//!   `config_version`)与 OAuth `/oauth/token`(刷新)。
//!
//! ## 防关联(PROTOCOL §7)
//!
//! 每号一套**冻结**的身份:token / machineId / macMachineId / session-id /
//! client-key / checksum / config-version,加**一个独立出口**。刷新与发包必须同出口 ——
//! 这条在本 crate 里由 [`CursorProvider::client_for`] 保证:`chat`、`GetServerConfig`、
//! `refresh_auth` 三条路径都从它取 client。
//!
//! 已支持:多轮(历史折叠)、tool_use 往返(参数含数字/布尔/对象/数组)、
//! thinking 透传、图像(内联)、PDF(我方抽文本层)、上游自报 usage。
//!
//! 未做:FileSyncService blob 上传(L2 文件附件)、
//! Cursor 内建工具的代执行(**有意不做**:那等于跑模型选定的 shell 命令)。

pub mod auth;
pub mod login;
mod chat;
mod config;
mod models;
mod pdf;
mod protobuf;
pub mod run;
pub mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::model::ModelInfo;
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, Provider};

/// config_version 缓存 TTL:真 IDE 按 poll interval(分钟级)刷新,取 2 分钟保守值,
/// 既避免每次 chat 都打一发 GetServerConfig,又能跟上服务端轮换。
/// `config_version` 缓存寿命。
///
/// 2026-08-07 上调 120s → 30min:实测 `GetServerConfig` 单次要 **5–6 秒**,而
/// 120s 的 TTL 在真实客户端(opencode 一次交互并发发好几个请求)下几乎每轮都过期,
/// 于是每个请求头上都白挂 5 秒。这个值是握手下发的配置版本,不是会过期的凭据,
/// 半小时一取足够。
const CONFIG_VERSION_TTL: Duration = Duration::from_secs(1800);

/// 一条 config_version 缓存记录。
///
/// `value: None` = **上次取失败了**(负缓存)。有它才能挡住「api2 持续故障时,
/// 每个请求都排进单飞闸门、各付一次完整超时」—— 那会把 worker 吞吐塌成 1 req / 5-6s。
struct ConfigEntry {
    value: Option<String>,
    at: Instant,
}

/// 取失败后多久不再重试。
///
/// 比 [`CONFIG_VERSION_TTL`] 短得多:失败是瞬态的,60 秒后该再试一次;
/// 但 60 秒内的请求不该各自再去撞一遍。
const CONFIG_FAIL_TTL: Duration = Duration::from_secs(60);

/// 账号未配时区时的缺省值。
const DEFAULT_TIMEZONE: &str = "Asia/Shanghai";

const CURSOR_ACCOUNT_SCHEMA: &[FieldSpec] = &[
    FieldSpec::new("account_id", "账号 ID", FieldType::String, true),
    FieldSpec::new("access_token", "Access Token", FieldType::Password, true)
        .with_help("Cursor session JWT;取自 state.vscdb 的 cursorAuth/accessToken"),
    FieldSpec::new("refresh_token", "Refresh Token", FieldType::Password, false)
        .with_help("取自 state.vscdb 的 cursorAuth/refreshToken。留空则无法自动续期,token 过期后该号会被判失效下线"),
    FieldSpec::new("machine_id", "Machine ID", FieldType::String, false)
        .with_help("checksum 用的 machineId;应填真 IDE 的 telemetry.machineId(64-hex,取自 storage.json);留空则按 sha256hex(token) 派生"),
    FieldSpec::new("mac_machine_id", "Mac Machine ID", FieldType::String, false)
        .with_help("真 IDE 的 telemetry.macMachineId(64-hex);留空则派生一个 —— 真客户端的 checksum 恒为 137 字符,缺了它长度对不上"),
    FieldSpec::new("config_version", "Config Version", FieldType::String, false)
        .with_help("x-cursor-config-version;留空则每会话现调 GetServerConfig 取新鲜值(推荐留空)"),
    FieldSpec::new("timezone", "时区", FieldType::String, false)
        .with_help("x-cursor-timezone,如 Asia/Shanghai / America/Los_Angeles。应与该号出口 IP 的地理位置一致,否则是关联特征。留空按 Asia/Shanghai"),
    FieldSpec::new("proxy", "出口代理", FieldType::String, false)
        .with_help("该账号专属出口(http/https/socks5)。防关联硬要求:推理、取 config、刷新 token 全走它。留空走 worker 默认出口"),
];

#[derive(Debug, Clone)]
pub struct CursorConfig {
    /// 推理主机(`AgentService/Run`)。
    pub agent_host: String,
    /// unary 服务主机(`GetServerConfig`)。
    pub api_host: String,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            agent_host: "agentn.api5.cursor.sh".to_string(),
            api_host: "api2.cursor.sh".to_string(),
        }
    }
}

impl CursorConfig {
    fn from_cfg(cfg: &serde_json::Value) -> Self {
        let c = cfg.get("cursor");
        let pick = |key: &str, dflt: &str| {
            c.and_then(|v| v.get(key))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(dflt)
                .to_string()
        };
        let d = CursorConfig::default();
        CursorConfig {
            agent_host: pick("agent_host", &d.agent_host),
            api_host: pick("api_host", &d.api_host),
        }
    }
}

/// 请求形状调优。
///
/// **生产一律用 [`Default`](RunTuning::default)**(= 与真客户端一致)。这组开关存在的
/// 唯一理由是协议试错:`PROTOCOL-agent-run.md` §0 承认从没做过删字段/删帧实验,
/// 「哪些字段必需」是空白,只能对着真上游二分。放在 API 上而不是读环境变量 ——
/// 上一版的 `CURSOR_METHOD` 环境变量开关就是这么让生产悄悄打错端点的。
#[derive(Debug, Clone, Copy, Default)]
pub struct RunTuning {
    pub shape: run::RunShape,
    /// 补发两个 `field 3` 上下文帧(真客户端初始 3 帧)。
    pub context_frames: bool,
    /// 发完初始帧后不 half-close 请求流(真客户端是 BiDi)。
    pub keep_stream_open: bool,
}

impl RunTuning {
    /// 与真客户端一致:全分节 + 3 帧 + 不关流。
    pub fn faithful() -> Self {
        Self {
            shape: run::RunShape::default(),
            context_frames: true,
            keep_stream_open: true,
        }
    }
}

pub struct CursorProvider {
    cfg: CursorConfig,
    /// 见 [`RunTuning`]。默认 [`RunTuning::faithful`]。
    tuning: RunTuning,
    /// worker 注入的默认出口 client(账号没配 proxy 时用)。
    egress_client: reqwest::Client,
    /// proxy URL → client 缓存。distinct 代理数很小(O(账号代理种类)),
    /// `reqwest::Client` 内部是 Arc,clone 廉价。
    ///
    /// 与 gw-kiro 的 `EgressResolver`、gw-dario 的 `proxy_clients` 是同一个模式 ——
    /// 本仓库既有做法是每个 provider 自管出口解析,不共用。
    proxy_clients: Mutex<HashMap<String, reqwest::Client>>,
    /// 缓存键 → 结果。见 [`config_cache_key`]:键含**身份指纹**,不只是 account_id。
    ///
    /// 后台把同一个 account_id 的 token / machine_id / proxy 换掉时,旧身份取回来的
    /// config_version 会继续被复用最长 30 分钟 —— 而那是一个服务端没见过的组合。
    /// 指纹进键之后,换凭据自动等于换缓存条目。
    config_cache: Mutex<HashMap<String, ConfigEntry>>,
    /// `GetServerConfig` 的 single-flight 闸门,**按缓存键分**。
    ///
    /// 一次取要 5–6 秒,而缓存冷启动时并发请求会各取一次,所以要单飞。
    /// 但**不能用一把全局锁**:那样 50 个各有独立代理的冷号会被串成 250–300 秒,
    /// 而它们本可以并行。更糟的是任一个号的代理卡住,整个 cursor 池一起排队。
    ///
    /// 用 async 锁而不是 std 锁:必须**跨 await 持有**才挡得住并发,而 std 锁跨 await
    /// 会把 future 变成 `!Send`(且长期持锁阻塞整个 runtime 线程)。
    config_gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 服务端已建立的会话。见 [`ConvRegistry`]。
    conversations: Arc<ConvRegistry>,
}

/// 会话在**服务端**是否已经建立,以及建在哪个号上。
///
/// ## 为什么必须记账号
///
/// Cursor 的会话历史由服务端按 `conversation_id` 持有,而它**属于某一个账号**。
/// 换号 = 失忆:新号那边根本没有这个会话。所以「同一会话继续」的前提是
/// **同一个账号**;账号不同就必须降级成首轮,把历史整个重铺一遍。
///
/// ## 为什么是「成功后才记」
///
/// 请求失败(限流/截断/空回复)时服务端很可能没有落下这一轮。乐观登记的话,
/// 下一次会用 `Continuation` 只发新消息 —— 而服务端那边缺了上一轮,
/// 表现是模型答非所问,且**没有任何错误**。宁可多铺一次历史。
#[derive(Default)]
pub(crate) struct ConvRegistry {
    inner: Mutex<HashMap<String, ConvEntry>>,
    /// 有状态会话是否启用。**构造时读一次**,不是每个请求读一次环境变量。
    ///
    /// 关闭时这张表**完全不写**:`confirm` 每次成功都插入 + 全表 `retain` 扫过期,
    /// 而关闭状态下没人会读它 —— 纯浪费,还把所有流的收尾串在同一把锁上。
    stateful: bool,
}

struct ConvEntry {
    account_id: String,
    at: Instant,
}

impl ConvRegistry {
    /// `CURSOR_STATEFUL=1` 才启用。见 [`ConvRegistry::stateful`]。
    fn from_env() -> Self {
        // 2026-08-08 起**默认开启**:后续轮只发新消息,历史由服务端按 `1.5` 自持。
        // 实测两个事实跨 4 轮全部记住,且缓存命中率 32.6% → 49.8%(单轮最高 98.7%)。
        // 之前默认关是因为后续轮会静默挂起,根因已定位(上下文声明被错误地挪到了
        // 会话级 `1.2.17`,见 PROTOCOL §17)。
        //
        // `CURSOR_STATEFUL=0` 退回每轮全量重铺 —— 留这条退路是因为
        // "服务端记不记得" 这件事我方无法验证,只能靠模型答得对不对间接判断;
        // 万一上游改了行为,关掉它至少还是正确的(只是贵)。
        let stateful = std::env::var("CURSOR_STATEFUL").as_deref() != Ok("0");
        if !stateful {
            tracing::warn!("CURSOR_STATEFUL=0:每轮重铺全量历史(正确但更贵,且吃不到上游缓存)");
        }
        Self {
            inner: Mutex::new(HashMap::new()),
            stateful,
        }
    }

    /// 锁:毒化即恢复。这张表是纯缓存,毒化不代表数据不可用;
    /// 而 `unwrap` 会让此后**每个** cursor 请求在必经路径上 panic = provider 整体下线。
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, ConvEntry>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// 会话记录的存活时长。超时即当作服务端已经忘了,降级重铺。
///
/// 取 2 小时是保守值:宁可多铺一次历史(代价=一次全量上下文),
/// 也不要拿着过期的记录发 `Continuation`(代价=模型看不到历史却毫无报错)。
const CONV_TTL: Duration = Duration::from_secs(2 * 3600);

impl ConvRegistry {
    /// 判定本次该用哪种形态。**任何不确定都返回 `Opening`** —— 降级只多花 token,
    /// 而错判 `Continuation` 会让模型丢失上下文且不报错。
    fn phase_for(&self, conversation_id: &str, account_id: &str) -> run::Phase {
        // 关掉时一律走 `Opening` + 历史折叠(见 `chat::fold_history`)——
        // 那条路更贵但同样正确。
        if !self.stateful {
            return run::Phase::Opening;
        }
        let map = self.map();
        match map.get(conversation_id) {
            Some(e) if e.account_id == account_id && e.at.elapsed() < CONV_TTL => {
                run::Phase::Continuation
            }
            Some(e) if e.account_id != account_id => {
                tracing::info!(
                    conversation_id,
                    old = %e.account_id,
                    new = %account_id,
                    "cursor 会话换号,降级重铺历史(服务端会话属于旧号)"
                );
                run::Phase::Opening
            }
            _ => run::Phase::Opening,
        }
    }

    /// 本轮**成功**收尾后登记:服务端现在持有这个会话了。
    fn confirm(&self, conversation_id: &str, account_id: &str) {
        // 关闭时没人读这张表,写它纯属浪费(且把所有流的收尾串在同一把锁上)。
        if !self.stateful {
            return;
        }
        let mut map = self.map();
        map.insert(
            conversation_id.to_string(),
            ConvEntry {
                account_id: account_id.to_string(),
                at: Instant::now(),
            },
        );
        // 顺手清过期项:这张表按会话增长,没人清就是内存泄漏。
        map.retain(|_, e| e.at.elapsed() < CONV_TTL);
    }

    /// 本轮失败:服务端可能没落下这一轮,下次从首轮重来。
    fn forget(&self, conversation_id: &str) {
        if !self.stateful {
            return;
        }
        self.map().remove(conversation_id);
    }
}

impl CursorProvider {
    pub fn new(cfg: CursorConfig) -> Self {
        Self::with_client(cfg, reqwest::Client::new())
    }

    pub fn with_client(cfg: CursorConfig, egress_client: reqwest::Client) -> Self {
        chat::warn_if_dump_enabled();
        Self {
            cfg,
            tuning: RunTuning::faithful(),
            egress_client,
            proxy_clients: Mutex::new(HashMap::new()),
            config_cache: Mutex::new(HashMap::new()),
            config_gates: Mutex::new(HashMap::new()),
            conversations: Arc::new(ConvRegistry::from_env()),
        }
    }

    /// 覆盖请求形状(协议试错用;生产别调)。
    pub fn with_tuning(mut self, tuning: RunTuning) -> Self {
        self.tuning = tuning;
        self
    }

    pub fn from_config(
        cfg: &serde_json::Value,
        egress_client: reqwest::Client,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        Ok(Arc::new(Self::with_client(
            CursorConfig::from_cfg(cfg),
            egress_client,
        )))
    }

    /// 该账号应当使用的 HTTP client。**fail-closed。**
    ///
    /// **三条上游路径(chat / GetServerConfig / refresh)必须全部经由本方法取 client。**
    /// 刷新走了别的出口而发包走代理,等于把两个 IP 绑到同一个号上,是已知的关联维度。
    ///
    /// ## 为什么代理构造失败必须拒绝,而不是回退默认出口
    ///
    /// 这里曾经是「构造失败就 warn 一声回退 `egress_client`」,理由是"配置写错不该让号
    /// 彻底不可用"。那个权衡是反的:账号配了 `proxy` 就是在声明"我要独占这个出口",
    /// 回退等于**把本该互相隔离的多个号并到同一个 IP 上**,而这是已实测的封号维度
    /// (同出口关联封禁 59.5% vs 独立代理 0%,见记忆 caio-egress-silent-direct-bug)。
    /// 一个号暂时不可用是**安全的失败**;一批号被关联封掉不是。
    ///
    /// gw-dario 早先的对抗审查已经就同一问题定过案(`client_for_proxy` 明确 fail-closed),
    /// 这里对齐它 —— 同一个安全边界不该因为 provider 不同就换个结论。
    fn client_for(&self, account: &Account) -> Result<reqwest::Client, UpstreamError> {
        let proxy = account
            .extra_str("proxy")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(url) = proxy else {
            return Ok(self.egress_client.clone());
        };
        // poison 用 into_inner 恢复:这把锁只护一张纯缓存,毒化不代表数据不可用,
        // 而 unwrap 会让此后每个请求都 panic(对齐 gw-dario 的写法)。
        {
            let cache = self
                .proxy_clients
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(c) = cache.get(url) {
                return Ok(c.clone());
            }
        }
        match build_proxy_client(url) {
            Ok(c) => {
                self.proxy_clients
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(url.to_string(), c.clone());
                Ok(c)
            }
            Err(e) => {
                tracing::error!(%url, account = %account.account_id,
                    "cursor 账号的出口代理无法构造,拒绝使用该号(绝不回退默认出口): {e}");
                Err(UpstreamError::new(
                    UpstreamErrorKind::BadRequest,
                    format!("cursor 账号 {} 的出口代理配置非法,拒绝发包(回退默认出口=关联封号风险)", account.account_id),
                ))
            }
        }
    }

    /// 取账号 token(必填)。
    fn token_of(account: &Account) -> Result<String, UpstreamError> {
        account
            .extra_str("access_token")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| UpstreamError::bad_request("cursor 账号缺少 access_token"))
    }

    fn opt_str(account: &Account, key: &str) -> Option<String> {
        account
            .extra_str(key)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// machineId:显式 > sha256hex(token) 派生。
    fn machine_id_of(account: &Account, token: &str) -> String {
        Self::opt_str(account, "machine_id").unwrap_or_else(|| wire::default_machine_id(token))
    }

    /// macMachineId:显式 > 派生。**不返回 `None`** —— 真客户端的 checksum 恒含它。
    fn mac_machine_id_of(account: &Account, token: &str) -> String {
        Self::opt_str(account, "mac_machine_id")
            .unwrap_or_else(|| wire::default_mac_machine_id(token))
    }

    fn timezone_of(account: &Account) -> String {
        Self::opt_str(account, "timezone").unwrap_or_else(|| DEFAULT_TIMEZONE.to_string())
    }

    /// 解析当前请求应回显的 `config_version`:
    /// 显式配置 > 缓存内未过期值 > 现调 GetServerConfig。
    async fn resolve_config_version(
        &self,
        account: &Account,
        client: &reqwest::Client,
        token: &str,
        machine_id: &str,
        mac_machine_id: &str,
    ) -> Result<String, UpstreamError> {
        if let Some(explicit) = Self::opt_str(account, "config_version") {
            return Ok(explicit);
        }
        let key = config_cache_key(account, token, machine_id);
        match self.cached_config_version(&key) {
            CacheLook::Fresh(v) => return Ok(v),
            CacheLook::RecentlyFailed => {
                // 负缓存命中:60 秒内刚失败过,别再排队各付一次 5–6 秒的超时。
                return Err(UpstreamError::network(
                    "cursor config_version 近期取失败(负缓存未过期),未发出推理请求".to_string(),
                ));
            }
            CacheLook::Miss => {}
        }

        // ── single-flight(按缓存键,不是全局)────────────────────────────────
        // 这一趟要 5–6 秒。没有闸门时,一次交互并发来的 N 个请求会各打一次
        // (实测 opencode 一轮发 4 个,四条 5 秒白等并行叠在一起)。
        // 但闸门**必须按号分**:一把全局锁会把 N 个各有独立代理的冷号串成 N×6 秒,
        // 而它们本可以并行;任一个号的代理卡住还会让整个池排队。
        // 拿到闸门后**必须重查缓存** —— 排在后面的那些,前面那位已经取回来了。
        let gate = {
            let mut gates = self.config_gates.lock().unwrap_or_else(|p| p.into_inner());
            // 表按「身份指纹」增长,换凭据会留下旧条目。数量级 = O(账号数×换凭据次数),
            // 每项是一把空 Mutex,泄漏量可忽略;真要清理应跟着账号生命周期走,不在这里猜。
            gates.entry(key.clone()).or_default().clone()
        };
        let _held = gate.lock().await;
        if let CacheLook::Fresh(v) = self.cached_config_version(&key) {
            return Ok(v);
        }

        tracing::debug!(account = %account.account_id, "cursor 现调 GetServerConfig 取 config_version");
        let fetched = config::fetch_config_version(
            client,
            &self.cfg.api_host,
            token,
            machine_id,
            Some(mac_machine_id),
            // 时区必须与推理请求一致。硬编码 Asia/Shanghai 会让同一个"客户端会话"的
            // unary 报上海、推理报账号配的时区 —— 一个**内部自相矛盾**的指纹,
            // 比配错更可疑(schema 里那句"应与出口 IP 地理位置一致"就白写了)。
            &Self::timezone_of(account),
        )
        .await;

        match fetched {
            Ok(fresh) => {
                tracing::debug!("cursor GetServerConfig 返回,长度 {}", fresh.len());
                self.store_config(&key, Some(fresh.clone()));
                Ok(fresh)
            }
            // ⚠️ **取不到时宁可让本次请求失败,也不能发空串。**
            //
            // 这里曾经是「退化到过期旧值,再退化到空串」,理由是"配置版本号不是凭据,
            // 一次 api2 抖动不该打挂聊天"。那个推理漏了下游:`config.rs` 自己的结论是
            // **回显空/过期的 config_version 会被完整性门以 `resource_exhausted` 软封**,
            // 而 `resource_exhausted` 在 `trailer_to_error` 里映射成 `QuotaExhausted`,
            // 到了调度层是**持久禁用、不自愈、要人工 reset**(scheduler `DisabledReason`)。
            //
            // 也就是说那个"不阻断请求"的善意退化,实际效果是把一次 api2 超时变成
            // **一个健康账号被永久禁用**;api2 连续抖动就是整个 cursor 池挨个阵亡。
            // 现在改成返回可重试错误:`Run` 还没发出去,gw-app 不算 committed,
            // 会换号重试(别的号有自己的 config_version 缓存)。
            //
            // 过期旧值也不再兜底 —— 它走的是同一条软封路径,只是概率低一点。
            // 未过期的缓存值在上面已经返回了,走到这里就是真的没有可用值。
            Err(e) => {
                self.store_config(&key, None); // 负缓存,见 CONFIG_FAIL_TTL
                tracing::warn!(
                    account = %account.account_id,
                    error = %e,
                    "cursor 取不到 config_version,本次请求按可重试失败返回(绝不发空串:会被软封成额度耗尽)"
                );
                Err(UpstreamError::network(format!(
                    "cursor 取 config_version 失败,未发出推理请求: {e}"
                )))
            }
        }
    }

    fn store_config(&self, key: &str, value: Option<String>) {
        self.config_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                key.to_string(),
                ConfigEntry {
                    value,
                    at: Instant::now(),
                },
            );
    }

    fn cached_config_version(&self, key: &str) -> CacheLook {
        let map = self
            .config_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let Some(e) = map.get(key) else {
            return CacheLook::Miss;
        };
        match &e.value {
            Some(v) if e.at.elapsed() < CONFIG_VERSION_TTL => CacheLook::Fresh(v.clone()),
            None if e.at.elapsed() < CONFIG_FAIL_TTL => CacheLook::RecentlyFailed,
            _ => CacheLook::Miss,
        }
    }
}

/// 缓存查询结果。
enum CacheLook {
    /// 有未过期的值。
    Fresh(String),
    /// 近期取失败过,负缓存未到期 —— 别再排队重试。
    RecentlyFailed,
    /// 没有可用信息,该去取。
    Miss,
}

/// config_version 的缓存键 = `account_id` + **身份指纹**。
///
/// 只用 account_id 的话:后台把同一个号的 token / machine_id / proxy 换掉之后,
/// 旧身份取回来的 config_version 会继续被复用最长 30 分钟 —— 而「新 token + 旧 config」
/// 是一个服务端没见过的组合,正好撞上完整性门。指纹进键 = 换凭据自动换条目。
fn config_cache_key(account: &Account, token: &str, machine_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.update(b"\x00");
    h.update(machine_id.as_bytes());
    h.update(b"\x00");
    h.update(
        account
            .extra_str("proxy")
            .unwrap_or_default()
            .as_bytes(),
    );
    let d = h.finalize();
    // 前 8 字节够区分了,键不必长。
    let mut fp = String::with_capacity(16);
    for b in &d[..8] {
        fp.push_str(&format!("{b:02x}"));
    }
    format!("{}\u{0}{fp}", account.account_id)
}

/// 按 proxy URL 构建 client。支持 http/https/socks5。
fn build_proxy_client(url: &str) -> anyhow::Result<reqwest::Client> {
    let proxy = reqwest::Proxy::all(url)?;
    Ok(reqwest::Client::builder()
        .proxy(proxy)
        .connect_timeout(Duration::from_secs(20))
        // 不设整请求超时:Run 是流式的,读完整个流可能很久。
        .build()?)
}

#[async_trait]
impl Provider for CursorProvider {
    fn family(&self) -> &'static str {
        "cursor"
    }

    fn account_schema(&self) -> &'static [FieldSpec] {
        CURSOR_ACCOUNT_SCHEMA
    }

    /// 加载期 fail-fast:token 必填,且**配了 proxy 就必须构造得出来**。
    ///
    /// 把代理校验放在这里而不是只等第一次 chat:worker 启动时就报出来,
    /// 比让一个配错代理的号静静躺在池里、直到真有客户请求打上去才失败要好
    /// (那时表现是"号在池里但每次都失败",而不是"这个号配置有问题")。
    fn validate_account(&self, account: &Account) -> Result<(), UpstreamError> {
        Self::token_of(account)?;
        self.client_for(account).map(|_| ())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        Ok(models::list())
    }

    /// 会话亲和键。**必须覆盖** —— 不覆盖的后果见
    /// [`chat::affinity_key_from_body`](crate::chat) 的文档:trait 默认的 `None` 会让
    /// worker 把 `CallCtx.session_id`/`cache_key` 装成空串,进而让上游 `1.5` 与
    /// 两把每会话密钥全部退化。
    ///
    /// 对 Cursor 而言这比对 kiro 更要紧:kiro 丢亲和只是丢前缀缓存命中,
    /// Cursor 的会话历史在**服务端且属于某一个账号**,换号即失忆。
    fn affinity_key(&self, req: &ChatRequest) -> Option<String> {
        chat::affinity_key_from_body(&req.body)
    }

    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        let token = Self::token_of(&ctx.account)?;
        let machine_id = Self::machine_id_of(&ctx.account, &token);
        let mac_machine_id = Self::mac_machine_id_of(&ctx.account, &token);
        let client = self.client_for(&ctx.account)?;

        let config_version = self
            .resolve_config_version(&ctx.account, &client, &token, &machine_id, &mac_machine_id)
            .await?;

        // conversation_id:优先 router 下发的 session_id(会话稳定),否则 cache_key,
        // 两者都空时**从请求体自行派生** —— 绝不让空串上线。
        //
        // ⚠️ 为什么要有最后这层兜底:worker 是拿 `affinity_key().unwrap_or_default()` 装
        // `session_id`/`cache_key` 的。只要 `affinity_key` 返回 `None`,这两个值就都是空串,
        // 而空 `1.5` + 每账号恒定的 blob/fs key 是真客户端不会有的形态(见
        // `chat::affinity_key_from_body`)。本 provider 现在覆盖了 `affinity_key`,
        // 但兜底仍留着:调度层将来若改口径,这里不能又静默退回空串。
        //
        // 过一遍 `conversation_uuid`:既保证非空,也把调度用的分组前缀折进哈希 ——
        // 那是 caio 内部命名空间,不该原样进上游报文。
        let material = if !ctx.session_id.is_empty() {
            ctx.session_id.clone()
        } else if !ctx.cache_key.is_empty() {
            ctx.cache_key.clone()
        } else {
            chat::affinity_key_from_body(&req.body).unwrap_or_else(|| {
                // 连内容都派生不出来(空消息体)→ 当作全新会话。
                tracing::debug!("cursor 无法派生 conversation_id,按新会话处理");
                uuid::Uuid::new_v4().to_string()
            })
        };
        let conversation_id = chat::conversation_uuid(&material);

        chat::chat_stream(
            client,
            chat::RunCtx {
                host: self.cfg.agent_host.clone(),
                token,
                machine_id,
                mac_machine_id: Some(mac_machine_id),
                config_version,
                timezone: Self::timezone_of(&ctx.account),
                phase: self
                    .conversations
                    .phase_for(&conversation_id, &ctx.account.account_id),
                conversation_id: conversation_id.clone(),
                shape: self.tuning.shape,
                context_frames: self.tuning.context_frames,
                keep_stream_open: self.tuning.keep_stream_open,
            },
            req,
            // 只在**成功收尾**后才登记会话已建立。失败时清掉,下次从首轮重铺 ——
            // 服务端很可能没落下这一轮,而错用 Continuation 是无声的上下文丢失。
            {
                let reg = self.conversations.clone();
                let account_id = ctx.account.account_id.clone();
                Some(Arc::new(move |ok: bool| {
                    if ok {
                        reg.confirm(&conversation_id, &account_id);
                    } else {
                        reg.forget(&conversation_id);
                    }
                }))
            },
        )
        .await
    }

    /// 用 `refresh_token` 换一份新凭据(标准 OAuth2,见 [`auth`])。
    ///
    /// 走该账号**专属出口**,与推理同 IP。刷新成功后 access/refresh 都更新 ——
    /// Cursor 的新 access_token 兼任新的 refresh_token。
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        let refresh_token = Self::opt_str(account, "refresh_token")
            // 没单独配 refresh_token 时退回 access_token:两者在 Cursor 侧本就是同一个
            // JWT(见 auth 模块),旧号只录了 access_token 也能续上。
            .or_else(|| Self::opt_str(account, "access_token"))
            .ok_or_else(|| {
                UpstreamError::new(
                    UpstreamErrorKind::TokenInvalid,
                    "cursor 账号既无 refresh_token 也无 access_token,无法刷新",
                )
            })?;

        let client = self.client_for(account)?;
        let fresh = auth::refresh(&client, &refresh_token).await?;

        let mut updated = account.clone();
        updated.extra.insert(
            "access_token".to_string(),
            serde_json::Value::String(fresh.access_token.clone()),
        );
        updated.extra.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(fresh.refresh_token),
        );

        // 写回 `expires_at`。**不写它的后果**:gw-app 的 `has_fresh_token` 对缺失该字段
        // 的号「视为永鲜」→ 从不主动刷新 → 每个过期号都要先吃一次 401/403 才被动刷,
        // 而 403 的分类本身就是雷区(出口 IP 被拦也是 403)。能不走到那步就别走。
        if let Some(exp) = auth::token_expires_at(&fresh.access_token) {
            updated.extra.insert(
                "expires_at".to_string(),
                serde_json::Value::String(auth::format_unix_utc(exp)),
            );
        } else {
            tracing::warn!(account = %account.account_id,
                "cursor 新 token 解不出 exp,不写 expires_at(gw-app 会当永鲜,靠 403 兜底)");
        }

        // token 变了 → client-key / session-id / 派生的 machineId 全跟着变。
        // 缓存的 config_version 是按旧身份取的,必须作废,否则新 token 配旧 config
        // 会是一个服务端没见过的组合。
        //
        // 缓存键已含身份指纹(见 `config_cache_key`),换 token 天然换条目;
        // 这里再把该号名下的旧条目清掉,免得它们在表里躺到过期。
        {
            let prefix = format!("{}\u{0}", account.account_id);
            self.config_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|k, _| !k.starts_with(&prefix));
        }
        tracing::info!(account = %account.account_id, "cursor token 已刷新");
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn acct(pairs: &[(&str, &str)]) -> Account {
        let mut e = BTreeMap::new();
        for (k, v) in pairs {
            e.insert(k.to_string(), serde_json::json!(v));
        }
        Account {
            account_id: "c1".into(),
            provider: "cursor".into(),
            max_concurrency: 2,
            disabled: false,
            extra: e,
        }
    }

    #[test]
    fn family_is_cursor() {
        assert_eq!(
            CursorProvider::new(CursorConfig::default()).family(),
            "cursor"
        );
    }

    #[test]
    fn default_hosts_are_the_run_endpoint_and_api2() {
        let d = CursorConfig::default();
        assert_eq!(d.agent_host, "agentn.api5.cursor.sh");
        assert_eq!(d.api_host, "api2.cursor.sh");
        // 退役端点的域名不该再作为推理主机
        assert_ne!(d.agent_host, "api2.cursor.sh");
    }

    #[test]
    fn from_config_reads_both_hosts_and_defaults_each() {
        let cfg = serde_json::json!({"cursor":{"agent_host":"agentn.global.api5.cursor.sh"}});
        let c = CursorConfig::from_cfg(&cfg);
        assert_eq!(c.agent_host, "agentn.global.api5.cursor.sh");
        assert_eq!(c.api_host, "api2.cursor.sh", "未配的那个走默认");

        let empty = CursorConfig::from_cfg(&serde_json::Value::Null);
        assert_eq!(empty.agent_host, "agentn.api5.cursor.sh");
        // 空串不算配置
        let blank = CursorConfig::from_cfg(&serde_json::json!({"cursor":{"agent_host":"  "}}));
        assert_eq!(blank.agent_host, "agentn.api5.cursor.sh");
    }

    #[test]
    fn schema_declares_credentials_and_anti_correlation_fields() {
        let p = CursorProvider::new(CursorConfig::default());
        let s = p.account_schema();
        let has = |n: &str| s.iter().any(|f| f.name == n);
        assert!(s.iter().any(|f| f.name == "access_token" && f.required));
        for f in ["refresh_token", "machine_id", "mac_machine_id", "timezone", "proxy"] {
            assert!(has(f), "schema 缺字段 {f}");
        }
        // 凭据必须是 Password 类型,别在后台明文回显
        for f in ["access_token", "refresh_token"] {
            let spec = s.iter().find(|x| x.name == f).unwrap();
            assert!(matches!(spec.field_type, FieldType::Password), "{f} 应为 Password");
        }
    }

    #[test]
    fn token_required_else_bad_request() {
        assert!(CursorProvider::token_of(&acct(&[])).is_err());
        assert_eq!(
            CursorProvider::token_of(&acct(&[("access_token", "tok")])).unwrap(),
            "tok"
        );
    }

    #[test]
    fn machine_ids_explicit_or_derived_and_always_present() {
        let a = acct(&[("access_token", "t"), ("machine_id", "MID")]);
        assert_eq!(CursorProvider::machine_id_of(&a, "t"), "MID");

        // 都留空时两个 id 都要派生出来,且**互不相同**(真机上是两个独立值)
        let b = acct(&[("access_token", "t")]);
        let mid = CursorProvider::machine_id_of(&b, "t");
        let mac = CursorProvider::mac_machine_id_of(&b, "t");
        assert_eq!(mid.len(), 64);
        assert_eq!(mac.len(), 64);
        assert_ne!(mid, mac);
        // 派生必须确定性:同号每次请求身份要冻结
        assert_eq!(mac, CursorProvider::mac_machine_id_of(&b, "t"));
    }

    #[test]
    fn derived_ids_produce_137_char_checksum_like_real_client() {
        // 真 IDE 的 checksum 恒为 137 字符;身份留空也不能缩水成 72。
        let a = acct(&[("access_token", "t")]);
        let mid = CursorProvider::machine_id_of(&a, "t");
        let mac = CursorProvider::mac_machine_id_of(&a, "t");
        assert_eq!(wire::checksum(&mid, Some(&mac)).len(), 137);
    }

    #[test]
    fn timezone_defaults_but_is_overridable() {
        assert_eq!(CursorProvider::timezone_of(&acct(&[])), "Asia/Shanghai");
        assert_eq!(
            CursorProvider::timezone_of(&acct(&[("timezone", "America/Los_Angeles")])),
            "America/Los_Angeles"
        );
    }

    #[test]
    fn client_for_falls_back_to_egress_without_proxy() {
        let p = CursorProvider::new(CursorConfig::default());
        // 无 proxy → 默认出口(不 panic,拿得到 client)
        let _ = p.client_for(&acct(&[("access_token", "t")]));
        // 非法 proxy → 回退而不是崩
        let _ = p.client_for(&acct(&[("access_token", "t"), ("proxy", "not a url")]));
        // 合法 proxy → 建得出来,且第二次命中缓存
        let a = acct(&[("access_token", "t"), ("proxy", "http://127.0.0.1:1080")]);
        let _ = p.client_for(&a);
        let _ = p.client_for(&a);
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 1);
    }

    #[test]
    fn validate_account_checks_token() {
        let p = CursorProvider::new(CursorConfig::default());
        assert!(p.validate_account(&acct(&[])).is_err());
        assert!(p.validate_account(&acct(&[("access_token", "tok")])).is_ok());
    }
}
