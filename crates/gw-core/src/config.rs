//! 配置 DTO(instances / accounts / system)。
//!
//! 配置驱动(借鉴 ALLinOne 的 YAML 形态)。三份配置:
//! - [`InstancesConfig`] 进程拓扑(多进程核心:router + 各 worker 出口/账号组)
//! - [`AccountsConfig`]  账号(按组分配到 worker)
//! - [`SystemConfig`]    运行开关(缓存/empty-fallback 等热调参数)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::account::Account;

// ───────────────────────── instances.yaml ─────────────────────────

/// 进程拓扑配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstancesConfig {
    pub router: RouterConfig,
    pub workers: Vec<WorkerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// 对外监听地址,如 `0.0.0.0:8990`。
    pub listen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// 实例号(与 `--instance N` 对应)。
    pub instance: u32,
    /// worker 监听地址(localhost 高位端口),如 `127.0.0.1:9000`。
    pub listen: String,
    /// 出口配置(本 worker 所有请求的固定出口)。
    pub egress: EgressConfig,
    /// 该 worker 管理的账号组名(对应 accounts.yaml 的 groups key)。
    pub account_group: String,
}

/// 出口配置 —— 多进程防关联的核心。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EgressConfig {
    /// 直连(系统默认出口)。
    Direct,
    /// 绑定本机源 IP(reqwest local_address)。单机双 IPv4 场景。
    LocalIp { address: String },
    /// 走外部代理(固定 IP 代理池)。
    Proxy {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
}

impl InstancesConfig {
    /// 按实例号查 worker 配置。
    pub fn worker(&self, instance: u32) -> Option<&WorkerConfig> {
        self.workers.iter().find(|w| w.instance == instance)
    }

    /// 拓扑约束校验(router/worker 启动时调用,违规直接拒绝启动):
    /// `account_group` 不得被多个 worker 绑定——账号运行态(并发槽/refresh 单飞/
    /// 冷却)都在单 worker 内存,两 worker 共享一组会让 max_concurrency 翻倍、
    /// rolling refresh_token 互相覆盖;instance 号与 listen 地址同理不得重复。
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut groups = std::collections::HashSet::new();
        let mut instances = std::collections::HashSet::new();
        let mut listens = std::collections::HashSet::new();
        for w in &self.workers {
            if !groups.insert(&w.account_group) {
                anyhow::bail!(
                    "instances.yaml 非法:账号组 '{}' 被多个 worker 绑定(并发与凭据刷新会互踩)",
                    w.account_group
                );
            }
            if !instances.insert(w.instance) {
                anyhow::bail!("instances.yaml 非法:instance={} 重复", w.instance);
            }
            if !listens.insert(&w.listen) {
                anyhow::bail!("instances.yaml 非法:listen '{}' 重复", w.listen);
            }
        }
        Ok(())
    }
}

// ───────────────────────── accounts.yaml ─────────────────────────

/// 账号配置(按组组织)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsConfig {
    /// 组名 → 组定义。
    pub groups: BTreeMap<String, AccountGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountGroup {
    /// 该组使用的 provider 家族。
    pub provider: String,
    /// 组内账号。
    pub accounts: Vec<Account>,
}

impl AccountsConfig {
    /// 取某组的账号列表。
    pub fn group(&self, name: &str) -> Option<&AccountGroup> {
        self.groups.get(name)
    }

    /// 取某组账号,并把组级 `provider` 传播到每个账号(账号级 provider 省略时)。
    pub fn group_accounts_with_provider(&self, name: &str) -> Option<Vec<Account>> {
        let g = self.groups.get(name)?;
        Some(
            g.accounts
                .iter()
                .cloned()
                .map(|mut a| {
                    if a.provider.is_empty() {
                        a.provider = g.provider.clone();
                    }
                    a
                })
                .collect(),
        )
    }
}

// ───────────────────────── system.yaml ─────────────────────────

/// 运行开关(热调参数,沿用旧项目语义)。
///
/// 注:空响应不设配置——v60 起不做任何反代侧重试/兜底(实战证明换 ID 重发救不回
/// 且 error 放大触发封号),行为固定为:provider 终态 Err(EmptyResponse) →
/// worker report_failure 阈值冷却 → 终态 SSE error → 客户端自重试。
/// 上游流式请求总超时(秒)的默认值。reqwest `.timeout()` 覆盖**整请求**含读完整个
/// 流式 body;Opus 大上下文常跑 300~700s,旧硬编码 300s 会被 reqwest 在 body 读取期
/// 腰斩,表现为 502「读取上游流失败: error decoding response body」(2026-06-16 实测:
/// caio 当天 33 次 stream_io 失败几乎全卡 `duration_ms≈300003`)。对齐 kiro.rs
/// `api_timeout_secs=720`(其在同上游同模型下基本不触顶)。0 视为未设,回落本默认。
pub const DEFAULT_UPSTREAM_TIMEOUT_SECS: u64 = 720;

fn default_upstream_timeout_secs() -> u64 {
    DEFAULT_UPSTREAM_TIMEOUT_SECS
}

/// 入站请求体体积上限(字节)的默认值。客户端 base64 图片/PDF 常使整请求体达数 MB;
/// axum 0.8 的 `Bytes`/`Json` 提取器默认上限仅 **2MB**,超了在 handler 执行前就被框架
/// 直接 413(且请求根本到不了业务逻辑,不入库、不可见——2026-06 线上实测)。取 **16MB**:
/// = 出站 6.3MB 护栏(gw-kiro 侧,对齐 Kiro 上游 ~7.3MB 硬限)的 ~2.5×,给当前轮 + 可被
/// shed 裁掉的历史媒体留足余量;同时是**有界**值(非 disable),防超大 body 在 router/worker
/// 入口无界缓冲撑爆内存(DoS——本网关 :38991 对外,入口提取在鉴权前完成)。8× 于旧 2MB 已
/// 决定性解除闷死;需更大可在 system.yaml 显式上调。0 视为未设,回落本默认。
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

fn default_max_request_body_bytes() -> usize {
    DEFAULT_MAX_REQUEST_BODY_BYTES
}

/// dario sidecar 连接配置(claude-dario provider 用)。
///
/// 空 `sidecar_url`/`api_key` 是合法默认值——provider 工厂收到后:
/// - `sidecar_url` 空 → 回落 `http://127.0.0.1:39100`;
/// - `api_key` 空 → dario 只在 loopback 放行(无 `DARIO_API_KEY` 时安全;
///   生产部署必须与 dario `DARIO_API_KEY` env 保持一致)。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DarioSidecarConfig {
    /// 本机 dario-on-Bun 监听地址,如 `http://127.0.0.1:39100`。
    pub sidecar_url: String,
    /// dario 入站鉴权 key(对应 sidecar `DARIO_API_KEY` env)。
    pub api_key: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemConfig {
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub image: ImageConfig,
    #[serde(default)]
    pub experimental: ExperimentalConfig,
    /// 上游流式请求总超时(秒)。详见 [`DEFAULT_UPSTREAM_TIMEOUT_SECS`]。
    /// `#[derive(Default)]` 会给 0——build_client 内把 0 视为未设回落默认,故 0 安全。
    #[serde(default = "default_upstream_timeout_secs")]
    pub upstream_timeout_secs: u64,
    /// 入站请求体体积上限(字节)。详见 [`DEFAULT_MAX_REQUEST_BODY_BYTES`]。
    /// **启动期一次性参数**:axum `DefaultBodyLimit` 在 app 构建时定,不随 30s overlay 热重载,
    /// 故不进 `SystemSettings`(避免"前端改了不重启不生效"的误导)。`#[derive(Default)]` 给 0,
    /// 经 [`effective_max_request_body_bytes`](Self::effective_max_request_body_bytes) 回落默认。
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
    /// dario sidecar(claude-dario provider)连接参数。
    /// **启动期参数**:worker 启动时一次性注入 provider 工厂;改后需重启相关 worker。
    #[serde(default)]
    pub dario: DarioSidecarConfig,
}

impl SystemConfig {
    /// 入站请求体上限的**有效值**:`0`(`#[derive(Default)]` 或显式置 0)回落
    /// [`DEFAULT_MAX_REQUEST_BODY_BYTES`],否则用配置值。router 与 worker 两个 axum app
    /// 构建处都调它来设 `DefaultBodyLimit::max(..)`。
    pub fn effective_max_request_body_bytes(&self) -> usize {
        if self.max_request_body_bytes == 0 {
            DEFAULT_MAX_REQUEST_BODY_BYTES
        } else {
            self.max_request_body_bytes
        }
    }
}

/// 实验性开关(默认关)。两个 on/off 可经设置面板热控;env(`KIRO_TOOLS_IN_PREFIX` /
/// `KIRO_CACHE_POINT`)作启动默认(后向兼容)。详见 `gw-kiro` converter/cache_point.rs。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentalConfig {
    /// 把工具定义放进 history[0] 前缀(蹭 Kiro 缓存)。⚠️ 实测会让部分客户端工具调用失效,
    /// 默认关。
    #[serde(default = "default_tools_in_prefix")]
    pub tools_in_prefix: bool,
    /// 把 Anthropic `cache_control` 翻成 Kiro `cachePoint`(实测 no-op,dormant)。
    #[serde(default = "default_cache_point")]
    pub cache_point: bool,
    /// 发**稳定** agentContinuationId(+agentTaskType="vibe")进 conversationState。默认关。
    /// 用于复刻 kiro.rs proven 配置做真实缓存命中的生产 A/B(见 gw-kiro converter/cache_point.rs)。
    #[serde(default = "default_agent_continuation")]
    pub agent_continuation: bool,
}

fn env_experimental_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}
fn default_tools_in_prefix() -> bool {
    env_experimental_flag("KIRO_TOOLS_IN_PREFIX")
}
fn default_cache_point() -> bool {
    env_experimental_flag("KIRO_CACHE_POINT")
}
fn default_agent_continuation() -> bool {
    env_experimental_flag("KIRO_AGENT_CONTINUATION")
}

impl Default for ExperimentalConfig {
    fn default() -> Self {
        Self {
            tools_in_prefix: default_tools_in_prefix(),
            cache_point: default_cache_point(),
            agent_continuation: default_agent_continuation(),
        }
    }
}

/// admin 控制面配置。`token` 未设(None / 空串)→ admin API 关闭(router 不挂 /admin)。
/// 与对外客户 apikey 完全分离:admin_token 是单一管理密钥(system.yaml 持有)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdminConfig {
    #[serde(default)]
    pub token: Option<String>,
}

impl AdminConfig {
    /// 非空 admin token(启用 admin 的充要条件)。
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref().filter(|t| !t.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub read_multiplier: f64,
    pub cap_ratio: f64,
    pub floor_ratio: f64,
    /// cache_sim 会话条目 TTL(秒);worker 启动时同步到全局 sim store。
    /// 带 serde 默认,旧 system.yaml(无此字段)仍可解析。
    #[serde(default = "default_sim_ttl_secs")]
    pub sim_ttl_secs: u64,
    /// cache_sim 最大会话数(LRU 容量);worker 启动时同步到全局 sim store。
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
}

fn default_sim_ttl_secs() -> u64 {
    300
}
fn default_max_sessions() -> usize {
    4096
}

/// 账号调度/冷却参数(worker 启动时注入 AccountScheduler)。
///
/// 默认值对齐 kiro.rs 生产配置(rateLimitCooldownSecs=300 / emptyResponse 60s·3次·60s窗 /
/// affinityMapTtlSecs=1800)。`max_failures` 例外:kiro.rs 是 3 但其 5xx/网络错误**不计数**
/// (重试同号);本项目 5xx/网络计入 failure_count 并换号,故默认放宽到 5,避免上游抖动
/// 误禁健康号(有全灭自愈兜底)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// 429 限流冷却秒数(到期自愈)。
    #[serde(default = "default_rate_limit_cooldown_secs")]
    pub rate_limit_cooldown_secs: u64,
    /// 账号临时封禁(TEMPORARILY_SUSPENDED)冷却秒数(默认 3600=1h,比限流长——别每 5min
    /// 重戳封禁号产生异常调用指纹)。到期自愈再试,仍封则再冷却。
    #[serde(default = "default_suspended_cooldown_secs")]
    pub suspended_cooldown_secs: u64,
    /// 空响应冷却秒数(达阈值后)。
    #[serde(default = "default_empty_response_cooldown_secs")]
    pub empty_response_cooldown_secs: u64,
    /// 空响应计数固定窗口秒数。
    #[serde(default = "default_empty_response_window_secs")]
    pub empty_response_window_secs: u64,
    /// 窗口内空响应达此次数才冷却(避免误伤偶发 empty 的健康号)。
    #[serde(default = "default_empty_response_threshold")]
    pub empty_response_threshold: u32,
    /// 连续失败(5xx/网络)达此次数自动禁用(TooManyFailures,可全灭自愈)。
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    /// 会话亲和条目 TTL 秒数(超时未访问 = 会话重开,自然再平衡)。
    #[serde(default = "default_affinity_ttl_secs")]
    pub affinity_ttl_secs: u64,
    /// worker 后台配额轮询开关(默认开)。复刻真实 Kiro IDE 每 ~5min 一次 getUsageLimits 的
    /// ambient 流量(防封)+ 让配额面板无人看 dashboard 时也新鲜。与冷却阈值同放 SchedulerConfig
    /// 是为复用既有 30s 热应用路径——admin 设置面板可即时启停轮询,无需重启(对抗审查 Architect#5)。
    #[serde(default = "default_quota_poll_enabled")]
    pub quota_poll_enabled: bool,
    /// 单个入站请求最多尝试的账号数(换号重试硬上限)。默认 **2**:一个失败请求最多波及 2 个号,
    /// 而非走遍全组——杜绝「毒请求/高频重试逐个打爆全池」(2026-06 大面积封号雪崩根因)。
    /// 内容/封禁类错误(见 `UpstreamErrorKind::worth_switching_account`)命中首个号即止,更不受影响。
    #[serde(default = "default_max_switch_attempts")]
    pub max_switch_attempts: u32,
}

fn default_rate_limit_cooldown_secs() -> u64 {
    300
}
fn default_suspended_cooldown_secs() -> u64 {
    3600
}
fn default_empty_response_cooldown_secs() -> u64 {
    60
}
fn default_empty_response_window_secs() -> u64 {
    60
}
fn default_empty_response_threshold() -> u32 {
    3
}
fn default_max_failures() -> u32 {
    5
}
fn default_quota_poll_enabled() -> bool {
    true
}
fn default_max_switch_attempts() -> u32 {
    2
}
fn default_affinity_ttl_secs() -> u64 {
    1800
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            rate_limit_cooldown_secs: default_rate_limit_cooldown_secs(),
            suspended_cooldown_secs: default_suspended_cooldown_secs(),
            empty_response_cooldown_secs: default_empty_response_cooldown_secs(),
            empty_response_window_secs: default_empty_response_window_secs(),
            empty_response_threshold: default_empty_response_threshold(),
            max_failures: default_max_failures(),
            affinity_ttl_secs: default_affinity_ttl_secs(),
            quota_poll_enabled: default_quota_poll_enabled(),
            max_switch_attempts: default_max_switch_attempts(),
        }
    }
}

/// 图像压缩参数(🔵 移植 kiro.rs/xkiro 的四档阈值;缩放规则对齐 Anthropic 官方建议)。
///
/// 多模态请求里 base64 原图会显著撑大请求体(撞上游字节上限/抬高成本),且恶意构造的
/// 解压炸弹可 OOM 整个 worker——压缩模块自带解码前护栏。`Copy`:压缩在 blocking
/// 线程池执行,按值携带配置。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ImageConfig {
    /// 总开关。关闭后所有图片原样透传,不解码不缩放(护栏也随之失效)。
    #[serde(default = "default_image_enabled")]
    pub enabled: bool,
    /// 长边像素上限,超过则等比缩放。
    #[serde(default = "default_image_max_long_edge")]
    pub max_long_edge: u32,
    /// 单图模式总像素上限。
    #[serde(default = "default_image_max_pixels_single")]
    pub max_pixels_single: u32,
    /// 多图模式总像素上限(图片数 ≥ multi_threshold 时生效)。默认与单图相等
    /// (开箱不产生差异,仅预留;对齐 xkiro 默认)。
    #[serde(default = "default_image_max_pixels_multi")]
    pub max_pixels_multi: u32,
    /// 多图档阈值:请求内图片数达此值即用多图像素上限。
    #[serde(default = "default_image_multi_threshold")]
    pub multi_threshold: usize,
}

fn default_image_enabled() -> bool {
    true
}
fn default_image_max_long_edge() -> u32 {
    4000
}
fn default_image_max_pixels_single() -> u32 {
    4_000_000
}
fn default_image_max_pixels_multi() -> u32 {
    4_000_000
}
fn default_image_multi_threshold() -> usize {
    20
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            enabled: default_image_enabled(),
            max_long_edge: default_image_max_long_edge(),
            max_pixels_single: default_image_max_pixels_single(),
            max_pixels_multi: default_image_max_pixels_multi(),
            multi_threshold: default_image_multi_threshold(),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            read_multiplier: 1.0,
            cap_ratio: 0.9,
            floor_ratio: 0.1,
            sim_ttl_secs: default_sim_ttl_secs(),
            max_sessions: default_max_sessions(),
        }
    }
}

// ───────────────────────── 热调设置 overlay(DB 持久) ─────────────────────────

/// DB 持久的**热调设置 overlay**:叠在 `system.yaml`([`SystemConfig`])基线之上。
///
/// 每个字段 `Option<T>`:`None` = 不覆盖(用 YAML 默认),`Some` = 覆盖。router 经
/// admin API 写库,worker 30s 轮询读库后 [`Self::apply_to`] 叠加并热应用——改参无需重启。
/// 进程拓扑(端口/每 worker 源 IP)**不在此**(留 instances.yaml,需重启)。
///
/// `default_proxy` 是唯一不进 [`SystemConfig`] 的字段:它不属于运行开关,而是出口选择,
/// 由 worker 单独取出注入 provider 的 egress resolver(账号无专属代理时的兜底出口)。
///
/// `deny_unknown_fields`:PUT 进来的设置若拼错 key(如 `max_failure`)直接拒绝,
/// 而非静默落库一个永不生效的死 overlay(对抗审查 Skeptic#5/Architect#8/Minimalist#2)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_proxy: Option<String>,
    /// 出口代理池(美国多 IP):导入/新建账号时按「当前分配最少」自动挑一个写进
    /// `account.extra.proxy`(粘性,每账号固定一个出口 IP,把账号均衡铺满 N 个 IP)。
    /// 与 `default_proxy` 一样**不进** [`SystemConfig`]:它不是运行开关,而是 admin
    /// 导入/新建/rebalance handler 直接读的分配源。空/None = 不自动分配。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_pool: Option<Vec<String>>,
    // —— cache ——
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_multiplier: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_cap_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_floor_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_sim_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_max_sessions: Option<usize>,
    // —— scheduler ——
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_cooldown_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_cooldown_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_response_cooldown_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_response_window_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_response_threshold: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failures: Option<u32>,
    /// 单请求换号重试硬上限(默认 2;反雪崩,见 [`SchedulerConfig::max_switch_attempts`])。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_switch_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_ttl_secs: Option<u64>,
    /// worker 后台配额轮询热开关(None = 用 yaml 基线默认 true)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_poll_enabled: Option<bool>,
    // —— image ——
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_max_long_edge: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_max_pixels_single: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_max_pixels_multi: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_multi_threshold: Option<usize>,
    // —— experimental ——
    /// 工具放 history[0] 前缀实验(默认关;⚠️ 部分客户端工具调用会失效)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_in_prefix: Option<bool>,
    /// cache_control→cachePoint 实验(实测 no-op,dormant)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_point: Option<bool>,
    /// 稳定 agentContinuationId+vibe 实验(真实缓存命中 A/B,默认关)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_continuation: Option<bool>,
}

impl SystemSettings {
    /// 把非 None 字段叠加到 `base`(原地)。`default_proxy` 不属 SystemConfig,不在此处理。
    pub fn apply_to(&self, base: &mut SystemConfig) {
        if let Some(v) = self.cache_read_multiplier { base.cache.read_multiplier = v; }
        if let Some(v) = self.cache_cap_ratio { base.cache.cap_ratio = v; }
        if let Some(v) = self.cache_floor_ratio { base.cache.floor_ratio = v; }
        if let Some(v) = self.cache_sim_ttl_secs { base.cache.sim_ttl_secs = v; }
        if let Some(v) = self.cache_max_sessions { base.cache.max_sessions = v; }
        if let Some(v) = self.rate_limit_cooldown_secs { base.scheduler.rate_limit_cooldown_secs = v; }
        if let Some(v) = self.suspended_cooldown_secs { base.scheduler.suspended_cooldown_secs = v; }
        if let Some(v) = self.empty_response_cooldown_secs { base.scheduler.empty_response_cooldown_secs = v; }
        if let Some(v) = self.empty_response_window_secs { base.scheduler.empty_response_window_secs = v; }
        if let Some(v) = self.empty_response_threshold { base.scheduler.empty_response_threshold = v; }
        if let Some(v) = self.max_failures { base.scheduler.max_failures = v; }
        if let Some(v) = self.max_switch_attempts { base.scheduler.max_switch_attempts = v; }
        if let Some(v) = self.affinity_ttl_secs { base.scheduler.affinity_ttl_secs = v; }
        if let Some(v) = self.quota_poll_enabled { base.scheduler.quota_poll_enabled = v; }
        if let Some(v) = self.image_enabled { base.image.enabled = v; }
        if let Some(v) = self.image_max_long_edge { base.image.max_long_edge = v; }
        if let Some(v) = self.image_max_pixels_single { base.image.max_pixels_single = v; }
        if let Some(v) = self.image_max_pixels_multi { base.image.max_pixels_multi = v; }
        if let Some(v) = self.image_multi_threshold { base.image.multi_threshold = v; }
        if let Some(v) = self.tools_in_prefix { base.experimental.tools_in_prefix = v; }
        if let Some(v) = self.cache_point { base.experimental.cache_point = v; }
        if let Some(v) = self.agent_continuation { base.experimental.agent_continuation = v; }
    }

    /// 由**有效** SystemConfig + 独立的 default_proxy 反构出全量(每字段都 Some)。
    /// admin `GET /settings` 用它把"有效值"回灌给前端(前端展示当前生效值)。
    pub fn from_effective(cfg: &SystemConfig, default_proxy: Option<String>) -> Self {
        Self {
            default_proxy,
            // egress_pool 不属 SystemConfig(同 default_proxy);from_effective 不重建它,
            // 调用方(settings::effective)从 overlay 原样回灌。
            egress_pool: None,
            cache_read_multiplier: Some(cfg.cache.read_multiplier),
            cache_cap_ratio: Some(cfg.cache.cap_ratio),
            cache_floor_ratio: Some(cfg.cache.floor_ratio),
            cache_sim_ttl_secs: Some(cfg.cache.sim_ttl_secs),
            cache_max_sessions: Some(cfg.cache.max_sessions),
            rate_limit_cooldown_secs: Some(cfg.scheduler.rate_limit_cooldown_secs),
            suspended_cooldown_secs: Some(cfg.scheduler.suspended_cooldown_secs),
            empty_response_cooldown_secs: Some(cfg.scheduler.empty_response_cooldown_secs),
            empty_response_window_secs: Some(cfg.scheduler.empty_response_window_secs),
            empty_response_threshold: Some(cfg.scheduler.empty_response_threshold),
            max_failures: Some(cfg.scheduler.max_failures),
            max_switch_attempts: Some(cfg.scheduler.max_switch_attempts),
            affinity_ttl_secs: Some(cfg.scheduler.affinity_ttl_secs),
            quota_poll_enabled: Some(cfg.scheduler.quota_poll_enabled),
            image_enabled: Some(cfg.image.enabled),
            image_max_long_edge: Some(cfg.image.max_long_edge),
            image_max_pixels_single: Some(cfg.image.max_pixels_single),
            image_max_pixels_multi: Some(cfg.image.max_pixels_multi),
            image_multi_threshold: Some(cfg.image.multi_threshold),
            tools_in_prefix: Some(cfg.experimental.tools_in_prefix),
            cache_point: Some(cfg.experimental.cache_point),
            agent_continuation: Some(cfg.experimental.agent_continuation),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dario_config_partial_only_sidecar_url_api_key_defaults_empty() {
        // 只提供 sidecar_url，省略 api_key → api_key 回落空串。
        let yaml = "dario:\n  sidecar_url: \"http://127.0.0.1:39100\"\n";
        let cfg: SystemConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.dario.sidecar_url, "http://127.0.0.1:39100");
        assert_eq!(cfg.dario.api_key, "");
    }

    #[test]
    fn dario_config_unknown_field_is_rejected() {
        // deny_unknown_fields：未知子字段应返回 Err，而非静默忽略。
        let yaml = "dario:\n  sidecar_url: \"http://127.0.0.1:39100\"\n  bogus_field: \"value\"\n";
        assert!(
            serde_yaml::from_str::<SystemConfig>(yaml).is_err(),
            "未知字段应被 deny_unknown_fields 拒绝"
        );
    }

    #[test]
    fn dario_config_defaults_and_parse() {
        // 缺省:两字段均为空串。
        let d = DarioSidecarConfig::default();
        assert_eq!(d.sidecar_url, "");
        assert_eq!(d.api_key, "");

        // SystemConfig 缺 dario 段 → 用默认(空串)。
        let cfg: SystemConfig = serde_yaml::from_str("upstream_timeout_secs: 600\n").unwrap();
        assert_eq!(cfg.dario.sidecar_url, "");
        assert_eq!(cfg.dario.api_key, "");

        // 解析 dario 段。
        let yaml = "dario:\n  sidecar_url: \"http://127.0.0.1:39100\"\n  api_key: \"local-key\"\n";
        let cfg2: SystemConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg2.dario.sidecar_url, "http://127.0.0.1:39100");
        assert_eq!(cfg2.dario.api_key, "local-key");
    }

    #[test]
    fn parse_instances_with_local_ip_and_proxy() {
        let yaml = r#"
router:
  listen: "0.0.0.0:8990"
workers:
  - instance: 0
    listen: "127.0.0.1:9000"
    egress: { mode: local_ip, address: "203.0.113.10" }
    account_group: "G0"
  - instance: 1
    listen: "127.0.0.1:9001"
    egress: { mode: proxy, url: "socks5://127.0.0.1:1080" }
    account_group: "G1"
"#;
        let cfg: InstancesConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.workers.len(), 2);
        match &cfg.worker(0).unwrap().egress {
            EgressConfig::LocalIp { address } => assert_eq!(address, "203.0.113.10"),
            _ => panic!("expected local_ip"),
        }
        match &cfg.worker(1).unwrap().egress {
            EgressConfig::Proxy { url, .. } => assert_eq!(url, "socks5://127.0.0.1:1080"),
            _ => panic!("expected proxy"),
        }
    }

    #[test]
    fn max_request_body_bytes_effective_and_default() {
        // #[derive(Default)] 给 0 → 回落默认(32MB)。
        let d = SystemConfig::default();
        assert_eq!(d.max_request_body_bytes, 0);
        assert_eq!(
            d.effective_max_request_body_bytes(),
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );
        assert_eq!(DEFAULT_MAX_REQUEST_BODY_BYTES, 16 * 1024 * 1024);

        // 显式 0 也回落默认(运维写 0 = 用默认,与 upstream_timeout_secs 同语义)。
        let mut z = SystemConfig::default();
        z.max_request_body_bytes = 0;
        assert_eq!(
            z.effective_max_request_body_bytes(),
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );

        // 显式非 0 值透传,不被回落。
        let mut c = SystemConfig::default();
        c.max_request_body_bytes = 5_000_000;
        assert_eq!(c.effective_max_request_body_bytes(), 5_000_000);

        // YAML 缺该字段 → serde 默认填 32MB(不是 0),且其它字段照常解析。
        let cfg: SystemConfig = serde_yaml::from_str("upstream_timeout_secs: 600\n").unwrap();
        assert_eq!(cfg.max_request_body_bytes, DEFAULT_MAX_REQUEST_BODY_BYTES);
        assert_eq!(cfg.upstream_timeout_secs, 600);
        // 显式写入则按写入值。
        let cfg2: SystemConfig =
            serde_yaml::from_str("max_request_body_bytes: 10485760\n").unwrap();
        assert_eq!(cfg2.max_request_body_bytes, 10_485_760);
        assert_eq!(cfg2.effective_max_request_body_bytes(), 10_485_760);
    }

    #[test]
    fn parse_accounts_groups() {
        let yaml = r#"
groups:
  G0:
    provider: kiro
    accounts:
      - { account_id: k1, refresh_token: t1 }
      - { account_id: k2, refresh_token: t2 }
"#;
        let cfg: AccountsConfig = serde_yaml::from_str(yaml).unwrap();
        let g = cfg.group("G0").unwrap();
        assert_eq!(g.provider, "kiro");
        assert_eq!(g.accounts.len(), 2);
        assert_eq!(g.accounts[0].extra_str("refresh_token"), Some("t1"));
    }

    #[test]
    fn instances_validate_rejects_duplicate_group() {
        let yaml = r#"
router: { listen: "0.0.0.0:8990" }
workers:
  - { instance: 0, listen: "127.0.0.1:9000", egress: { mode: direct }, account_group: "G0" }
  - { instance: 1, listen: "127.0.0.1:9001", egress: { mode: direct }, account_group: "G0" }
"#;
        let cfg: InstancesConfig = serde_yaml::from_str(yaml).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("G0"), "应指出重复组,实际: {err}");
    }

    #[test]
    fn instances_validate_accepts_distinct_topology() {
        let yaml = r#"
router: { listen: "0.0.0.0:8990" }
workers:
  - { instance: 0, listen: "127.0.0.1:9000", egress: { mode: direct }, account_group: "G0" }
  - { instance: 1, listen: "127.0.0.1:9001", egress: { mode: direct }, account_group: "G1" }
"#;
        let cfg: InstancesConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn system_config_defaults() {
        let cfg = SystemConfig::default();
        assert_eq!(cfg.cache.read_multiplier, 1.0);
    }

    #[test]
    fn system_config_ignores_legacy_empty_response_section() {
        // 旧 system.yaml 可能残留 v58 的 empty_response 段,解析必须兼容(忽略)。
        let yaml = "cache:\n  read_multiplier: 1.0\n  cap_ratio: 0.9\n  floor_ratio: 0.1\nempty_response:\n  buffered_fallback: true\n";
        let cfg: SystemConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.cache.cap_ratio, 0.9);
    }

    #[test]
    fn settings_apply_to_overwrites_only_present_fields() {
        let mut base = SystemConfig::default();
        let before_cooldown = base.scheduler.empty_response_cooldown_secs;
        let s = SystemSettings {
            max_failures: Some(1),
            cache_read_multiplier: Some(2.5),
            image_enabled: Some(false),
            ..Default::default()
        };
        s.apply_to(&mut base);
        // 覆盖的字段变了。
        assert_eq!(base.scheduler.max_failures, 1);
        assert_eq!(base.cache.read_multiplier, 2.5);
        assert!(!base.image.enabled);
        // 未提供的字段不动。
        assert_eq!(base.scheduler.empty_response_cooldown_secs, before_cooldown);
        assert_eq!(base.cache.cap_ratio, 0.9);
        // default_proxy 不进 SystemConfig(此处仅确认 apply_to 不 panic 即可)。
    }

    #[test]
    fn settings_from_effective_is_identity_under_apply_to() {
        // from_effective(默认配置) 再 apply_to 回默认配置,应得到同一份配置。
        let base = SystemConfig::default();
        let full = SystemSettings::from_effective(&base, Some("socks5://h:1".into()));
        assert_eq!(full.default_proxy.as_deref(), Some("socks5://h:1"));
        let mut target = SystemConfig::default();
        // 先扰动,再用 full 叠回,验证全字段都被 Some 覆盖。
        target.scheduler.max_failures = 999;
        target.cache.read_multiplier = 999.0;
        full.apply_to(&mut target);
        assert_eq!(target.scheduler.max_failures, base.scheduler.max_failures);
        assert_eq!(target.cache.read_multiplier, base.cache.read_multiplier);
    }
}
