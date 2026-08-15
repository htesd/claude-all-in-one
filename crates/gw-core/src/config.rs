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

/// 自动补货(drop.kiro.ss 买 Kiro `ksk_` 号并自动上号)的**启动期**配置。
///
/// 这里只放三样:参与开关、上游地址、密钥。**运行时可调的策略参数一律不在这里**
/// (水位/高峰窗口/日上限/预测参数……),它们存 control.db,面板改完即时生效无需重启。
///
/// 为什么这么切:热调参数的自然去处是 [`SystemSettings`],但那个结构体有一条**回滚地板**
/// (见其注释)——给它加字段会让回滚到 2026-07-31 之前的镜像变成全量 503。补货参数走自己的
/// 表天然避开,同时密钥也不会经 `GET /admin/api/settings` 回显给前端。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RestockConfig {
    /// 本进程是否参与补货。**默认 false** —— 没显式打开就绝不会有任何进程去花钱。
    ///
    /// 注意它不是"业务开关"(那个在面板上、存 DB),而是"这个二进制允不允许跑补货循环"。
    /// 生产上多个 router 共用同一份 system.yaml,所以打开它之后**由 DB 租约决定谁真正跑**,
    /// 不能靠这个字段做互斥。
    pub enabled: bool,
    /// drop 平台地址。空 → 回落 [`Self::DEFAULT_BASE_URL`]。
    pub base_url: String,
    /// drop 平台的 `X-API-Key`(`usr-` 开头)。空 → 视为未配置,循环不启动。
    pub api_key: String,
}

impl RestockConfig {
    pub const DEFAULT_BASE_URL: &'static str = "https://drop.kiro.ss";

    /// 有效地址:空串回落默认,并去掉尾斜杠(拼路径时避免 `//api/...`)。
    pub fn base_url(&self) -> &str {
        let s = self.base_url.trim().trim_end_matches('/');
        if s.is_empty() {
            Self::DEFAULT_BASE_URL
        } else {
            s
        }
    }

    /// 配置是否完整到可以启动补货循环。
    ///
    /// fail-closed:`enabled` 打开但密钥是空的 → **不启动**(而不是带着一个必然 401 的
    /// 客户端空转),让启动日志把这件事说出来。
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.api_key.trim().is_empty()
    }
}

/// 思考强度档位。**闭集,由低到高**,序列化为小写串(与上游 enum 逐字一致)。
///
/// 为什么是枚举而不是 `String`:这个值会**原样进 wire**
/// (`additionalModelRequestFields.effort`),上游只认这五个串。做成 `String` 时非法值在
/// 每一层都是可表示的 —— 对抗审查三个 lens 同时指出后果:`system.yaml` 里写
/// `default_effort: hihg` 能解析成功、`GET /settings` 照实返回 `hihg`,而数据面的消费点拒收
/// 并继续用旧值,于是**控制面显示值与实际生效值永久分叉**,面板上五个档位一个都不高亮,
/// 运维每 30 秒收到一次告警却看不出该改哪里。改成枚举后 serde 在配置装载与
/// `PUT /settings` 两处的边界上直接拒绝非法值,这个分叉状态不可达。
///
/// 档位**全集**在这里;"某个模型有没有这一档"是另一件事,见
/// `gw_kiro::converter::clamp_effort_for_model`(4.6 系没有 `xhigh`,4.5 系与 haiku 一档都没有)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingEffort {
    /// 由低到高的全集。`gw_kiro::anthropic_types::VALID_EFFORTS` 有一条用例钉住两者逐项相等。
    pub const ALL: [ThinkingEffort; 5] = [
        ThinkingEffort::Low,
        ThinkingEffort::Medium,
        ThinkingEffort::High,
        ThinkingEffort::Xhigh,
        ThinkingEffort::Max,
    ];

    /// wire 形态(小写)。`const` 以便在 const 上下文里取 [`DEFAULT_THINKING_EFFORT`] 的串。
    pub const fn as_str(&self) -> &'static str {
        match self {
            ThinkingEffort::Low => "low",
            ThinkingEffort::Medium => "medium",
            ThinkingEffort::High => "high",
            ThinkingEffort::Xhigh => "xhigh",
            ThinkingEffort::Max => "max",
        }
    }
}

/// 客户端**未指定** effort 时发给上游的默认思考档位。
///
/// **这里是唯一事实源** —— `gw_kiro::anthropic_types::DEFAULT_EFFORT` 是对它的别名。
/// 放在 gw-core 是因为它同时是**配置 schema 的默认值**([`ThinkingConfig`])和 adapter 的
/// 兜底档位;两处各写一份必然漂移。
///
/// 深度与延迟的取舍依据见 `gw_kiro::anthropic_types::DEFAULT_EFFORT` 的文档表格。
pub const DEFAULT_THINKING_EFFORT: ThinkingEffort = ThinkingEffort::High;

/// thinking(思维链)策略参数。可经设置面板热控(worker 30s 轮询生效)。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThinkingConfig {
    /// 客户端未带 `output_config.effort` 时用哪个档位。见 [`DEFAULT_THINKING_EFFORT`]。
    ///
    /// 只影响**没说话**的客户端:显式点了档位的请求原样透传(非法值除外)。
    pub default_effort: ThinkingEffort,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self { default_effort: DEFAULT_THINKING_EFFORT }
    }
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
    /// thinking(思维链)策略。见 [`ThinkingConfig`]。
    #[serde(default)]
    pub thinking: ThinkingConfig,
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
    /// 自动补货(drop.kiro.ss 买 ksk_ 号并上号)。**启动期参数 + 密钥**;
    /// 策略参数在 DB 里可热改。见 [`RestockConfig`]。
    #[serde(default)]
    pub restock: RestockConfig,
    /// **热追加/覆盖的 cursor 模型目录**(默认空 = 纯内置目录)。
    ///
    /// cursor 上游不提供菜单查询,模型表只能内置;但「新模型能不能用」又只能靠实测。
    /// 本字段让试探新模型**不用重新部署**:DB overlay 热改后 30s 内 worker 生效。
    /// `menu=false`(默认)= 探测位:可被点名、出现在 `/v1/models`,但不进每个
    /// 请求都带的 `1.14` 清单 —— 热配置只允许**试**,菜单位要回代码对齐真机快照
    /// 才能转正。同名条目整体覆盖内置项(参数一起换)。
    #[serde(default)]
    pub cursor_extra_models: Vec<ExtraModelSpec>,
    /// **cursor 内建工具护栏的策略句**(默认空 = 用 gw-cursor 内置默认)。
    ///
    /// Cursor 的内建工具(终端、读写文件、代码库检索、网页搜索)是服务端自带的,
    /// 哪怕我方一个工具都不声明模型照样会调,而反代执行不了 —— 只能收口,
    /// 表现成「半截回答」。护栏是追加在系统提示末尾的一段话,生产实测 12 小时里
    /// 仍有 302 次收口(296 次发生在已出字之后),所以文案要能反复调。
    ///
    /// 只有**策略句**在这里(「清单里没有的能力怎么办」这类自然语言);
    /// 工具闭集与能力替代表由 gw-cursor 按每次请求的 `tools` **代码生成**,
    /// 配置里没有占位符 —— 模板拼错会静默丢掉整个闭集,而闭集是这道护栏最硬的一半。
    ///
    /// **命名空间**:与 [`Self::cursor_extra_models`] 一致的 `cursor_` 前缀。
    /// 这个旋钮只影响 cursor 家族,kiro / claude-dario / claude-subprocess 一律不读它。
    #[serde(default)]
    pub cursor_tool_guard: String,
}

/// 一个热追加的 cursor 模型条目(见 [`SystemConfig::cursor_extra_models`])。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraModelSpec {
    /// Cursor Run 侧模型名(线上名,如 `grok-4.7`)。
    pub name: String,
    /// 参数 `{key: val}`(照抄同族模型的 parameterDefinitions,如 effort/fast/thinking)。
    #[serde(default)]
    pub params: Vec<(String, String)>,
    /// 进不进 `1.14` 可用清单。默认 **false** = 探测位(可被点名但不进清单)。
    #[serde(default)]
    pub menu: bool,
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

/// 实验性开关。`tools_in_prefix`/`cache_point`/`agent_continuation`/`q_endpoint` 默认 **关**;
/// `thinking_signature` 默认 **开**(保留现状,见其字段注释)。均可经设置面板热控;env
/// (`KIRO_TOOLS_IN_PREFIX` / `KIRO_CACHE_POINT` / `KIRO_AGENT_CONTINUATION` /
/// `KIRO_THINKING_SIGNATURE` / `KIRO_Q_ENDPOINT`)作启动默认(后向兼容)。详见 `gw-kiro` converter/cache_point.rs。
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
    /// 是否给响应 thinking 块附 `signature`。**默认开**(保留现状)。多上游反代场景关掉:caio 的
    /// Kiro 合成签名对真 Anthropic/Bedrock 验签非法,跨通道漂移会触发 `THINKING_SIGNATURE_INVALID`
    /// (见 gw-kiro converter/cache_point.rs::thinking_signature_enabled)。env `KIRO_THINKING_SIGNATURE=0` 关。
    #[serde(default = "default_thinking_signature")]
    pub thinking_signature: bool,
    /// 主推理上游端点选择。默认 **关**=`runtime.{region}.kiro.dev`(现状,防封对齐 static_flow
    /// 当前客户端);开=`q.{region}.amazonaws.com`(旧 CodeWhisperer 端点,与 kiro.rs 一致)。
    /// 【为何是开关】线上实测:runtime.kiro.dev 端点**真实 prompt 缓存命中 0%**、每 token 计费 ~2x;
    /// kiro.rs 走 q.amazonaws.com 端点真实命中 82-92%(报文/账号/亲和完全一致,唯一变量是端点)。
    /// ⚠️ 切旧端点更省积分,但客户端指纹偏离当前 Kiro,理论封号风险略升(kiro.rs 长期用它在跑);
    /// 可经设置面板热切,出问题一键切回。env `KIRO_Q_ENDPOINT=1` 作启动默认。
    #[serde(default = "default_q_endpoint")]
    pub q_endpoint: bool,
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
fn default_q_endpoint() -> bool {
    env_experimental_flag("KIRO_Q_ENDPOINT")
}
/// thinking 签名默认 **开**(现状),仅 env 显式设 `0`/`false` 才关。
fn default_thinking_signature() -> bool {
    std::env::var("KIRO_THINKING_SIGNATURE")
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true)
}

impl Default for ExperimentalConfig {
    fn default() -> Self {
        Self {
            tools_in_prefix: default_tools_in_prefix(),
            cache_point: default_cache_point(),
            agent_continuation: default_agent_continuation(),
            thinking_signature: default_thinking_signature(),
            q_endpoint: default_q_endpoint(),
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
    /// 账号临时封禁(TEMPORARILY_SUSPENDED)的**基准**冷却秒数(默认 3600=1h)。
    /// 实际冷却按连续 suspend 次数指数退避:第 k 次 = 本值 * 2^(k-1),
    /// 封顶 [`Self::suspended_backoff_cap_secs`],并叠 ±20% 抖动打散整批复活的同步性。
    #[serde(default = "default_suspended_cooldown_secs")]
    pub suspended_cooldown_secs: u64,
    /// 封禁冷却的退避上限秒数(默认 86400=24h)。
    #[serde(default = "default_suspended_backoff_cap_secs")]
    pub suspended_backoff_cap_secs: u64,
    /// 同一账号**连续** suspend 复活失败达此次数 → 自动退役(disabled=1 落库,
    /// 面板可见、可手动恢复;成功一次 chat 即清零)。**0 = 不自动退役**(恢复旧行为:
    /// 死号永远每小时冷却循环)。默认 3:实测 suspend 号 0/35 恢复,整点复活风暴只会
    /// 把客户原文反复泼向死号、持续喂给上游风控"这些身份同属一个池"的关联证据。
    #[serde(default = "default_suspend_retire_strikes")]
    pub suspend_retire_strikes: u32,
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
    /// 会话亲和「粘性预算」:primary **仅因并发满**而暂不可选时,等它腾出 permit 的
    /// 最长毫秒数。`0` = **只关掉等待**。
    ///
    /// ⚠️ `0` **不等于回到本开关引入前的行为**(对抗评审第五轮 [高],两人都指出这句
    /// 原文档在撒谎)。同批上线的**固定溢出伙伴**没有开关、始终生效:primary 仅因并发满
    /// 时会话**保留 primary、改走伙伴**,而引入前是「把伙伴当场转正、永不迁回」。
    /// 于是 `hold=0` 下 A 满→落 B→A 空出后仍回 A,老实现则会一直留在 B。
    /// 这个差异是**有意的**(它正是本次改造要解决的问题),这里只是把话说准。
    ///
    /// 为什么需要(2026-08-14 实测):轮转派号会让**同一会话的相邻两轮落到不同账号**,
    /// 上游看到的是「一个用户在恢复它从未创建过的数百轮对话」——这是账号池最强的
    /// 可检测特征。当天一批 10 个号 1 小时内被 `TEMPORARILY_SUSPENDED` 掉 9 个,
    /// 被封号平均 **27.5 会话/小时、73% 换会话率**,存活号 **9.4 / 36%**,无交叉。
    ///
    /// **只钳「并发满」这一种原因**:真失效(禁用/限流/RPM/额度/不支持本模型)照旧改选。
    /// 等待不消耗换号预算,预算耗尽即 fail-open 回落原路径,绝不把请求挂死。
    #[serde(default = "default_affinity_hold_ms")]
    pub affinity_hold_ms: u64,
    /// 是否允许把**已钉扎的会话**向上迁移到更高优先级层(默认 `true` = 引入本开关前的行为)。
    /// 关掉能进一步压低换会话率,代价是高优先层的便宜号被利用得慢一些。
    #[serde(default = "default_affinity_migrate_up")]
    pub affinity_migrate_up: bool,
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
    /// **排队等冷却**的最长等待毫秒数。`0` = 关闭(与本开关引入前逐字节等价)。
    ///
    /// 背景(2026-07-31):企业号的上游并发是**跨租户共享**的,一堆人在抢同一个池子,
    /// 429 是竞争而非我方过载。此时二值冷却是纯亏——我们一退,槽位立刻被别人吃掉,
    /// 而客户拿到 503。开启后:`acquire` 在「全禁用」时不立刻报错,只要还有号处于
    /// **冷却态**(429/空响应/临时封禁,即 `disabled_until` 有值)且会在预算内到期,
    /// 就等它自愈再选,把限速在网关内部消化掉,客户端感知不到。
    ///
    /// ⚠️ **额度跑干的号不产生等待**:`QuotaExhausted` 的 `disabled_until` 是 `None`,
    /// 不构成"等得到"的理由。所以池子真的干了仍然快速失败,不会把客户挂满整个预算。
    /// 上限必须**远小于**下游客户端的空闲判定(yapi 为 300s 无 event 即中止),
    /// 且这段等待发生在**响应开始之前**,客户端只是变慢,不会看到半截流。
    #[serde(default = "default_queue_wait_ms")]
    pub queue_wait_ms: u64,
    /// **限流节流间隔**(毫秒),只作用于开了 `extra.queue_enabled` 的号。`0` = 关闭,
    /// 429 仍走二值冷却(与本开关引入前逐字节等价)。
    ///
    /// 背景:企业号的上游并发跨租户共享,429 是**竞争**而非我方过载。二值冷却会把号
    /// 整个拉出轮转 `rate_limit_cooldown_secs` 秒,期间请求转去烧别的号 —— 在企业号
    /// 持有大部分剩余额度时,这等于拿稀缺额度替竞争买单。改成节流后:号**不下线**,
    /// 只是本次 429 后 `pace` 毫秒内不再被选中,到点继续抢 —— 即"保持一个频率访问"。
    ///
    /// ⚠️ 定频探测正是历史上 22 分钟送走 5 个号的模式,所以配套有熔断:
    /// 连续 [`Self::rate_limit_pace_max_strikes`] 次 429 仍未成功,退回二值冷却。
    #[serde(default = "default_rate_limit_pace_ms")]
    pub rate_limit_pace_ms: u64,
    /// 节流熔断阈值:同一账号**连续**这么多次 429 后放弃节流、退回二值冷却。
    /// **`0` = 不熔断(默认)**:开了排队的号**永远只节流、不因 429 下线**。
    ///
    /// 为什么默认不熔断:上游其实给了两种不同的信号 —— 429 是「槽位被别的租户占了」
    /// (竞争,与本号健康无关),403 `TEMPORARILY_SUSPENDED` 才是「你被惩罚了」。后者走
    /// 独立分支(1h 冷却 + 不换号),已经是明确的下线信号,不需要再拿 429 的连击去**猜**。
    /// 用 429 猜的代价是实打实的:号被拉出轮转的那几秒,请求转去烧别的号,而企业号往往
    /// 正握着大部分剩余额度。无界硬撞的风险由**请求级 180s 总时限** + `pace` 频率上限兜底。
    ///
    /// 若日后真的观察到 suspend 增多,把它调回 10 左右即可恢复熔断(热调,不用重启)。
    #[serde(default = "default_rate_limit_pace_max_strikes")]
    pub rate_limit_pace_max_strikes: u32,
    /// **降层前为高优先层等待**的最长毫秒数。`0` = 关闭(与本开关引入前逐字节等价)。
    ///
    /// 背景(2026-08-04):自购速刷号挂在组内最高优先层,单号被 429 节流的那几百毫秒里,
    /// `eligible_ids` 会把它剔出合格集,请求于是**立刻降到低优先层的兜底号**。实测一个
    /// 30 分钟窗口:速刷号吃 1082 个请求,兜底号吃 169 个,而这 169 个与 429 次数分钟级
    /// 近乎 1:1 —— 零 429 的 13 分钟里兜底号精确为 0。这不是容量问题(在途并发 4–13,
    /// 上限 100),纯粹是「不肯等 250ms」。
    ///
    /// 开启后:若更高优先层存在**仅因 429 节流**而暂不可选的号,就在预算内等它回来,
    /// 而不是降层。等待发生在**响应开始之前**,客户端只是变慢、不会看到半截流;顺带
    /// 保住 prompt 缓存亲和(换号意味着 cache_read 全部重算)。
    ///
    /// ⚠️ **只等节流,不等冷却**:禁用/冷却态的号由 [`Self::queue_wait_ms`] 负责,两者
    /// 职责不重叠。预算用尽一律降层兜底,**绝不因此把错误抛给客户**。
    #[serde(default = "default_tier_hold_ms")]
    pub tier_hold_ms: u64,
    /// **请求级窗口**:一个请求开始后多少毫秒内,它的取号还允许触发上面的等待。
    /// 超出即照常降层。`0` = 关闭。
    ///
    /// 为什么必须封顶:`switch_cap(RateLimited)` 是 `total`(全组),配 180s 的请求级
    /// 总时限,若不封,一个持续撞 429 的请求能在同一个高优先号上来回弹几百次 ——
    /// 把「省钱」变成「客户干等」。窗口远小于 180s,等待也就不可能把响应推到那条硬线附近。
    ///
    /// ⚠️ 为什么是**时间窗口**而不是「等几轮」:换号重试的轮数计数器
    /// (`worker::messages` 的 `attempts`)对**所有**失败类别共用 —— 凭证刷新失败、
    /// `ModelNotAvailable` 都会消耗它。拿它当等待额度,会出现「前两轮被无关错误吃掉,
    /// 真撞上 429 时反而不许等」的反直觉行为,而这正是本开关要治的病。时间窗口只
    /// 依赖墙上时钟,不受失败类别干扰。
    #[serde(default = "default_tier_hold_window_ms")]
    pub tier_hold_window_ms: u64,
    /// **低优先新号暖机**总开关(默认开):调度 rank >= 100 的号按号龄(accounts.created_at)
    /// 获得更低的有效 RPM 上限;rank < 100 的高优号(成员边 @0 的 POWER/PRO MAX 等)
    /// 完全不受影响。背景:restock 补货的新号一上线就被鲸客流量瞬时灌满
    /// (2026-08-10 ha7477062:新号几分钟内打到封号),上游对新号的节奏容忍远低于老号。
    ///
    /// 有效 RPM = min(账号 `extra.rpm_limit`, 暖机上限) —— 暖机只收紧、**绝不放宽**
    /// 账号已有更严的上限。挂在现有 rpm_limit 滑动窗口语义上:达限后的等待/迁移/503
    /// 行为与既有定频完全一致,不改并发、不改排序、不改会话亲和。
    #[serde(default = "default_warmup_enabled")]
    pub warmup_enabled: bool,
    /// 适应期时长(小时):号龄 < 本值,有效 RPM = [`Self::warmup_phase1_rpm`]。0 = 跳过该期。
    #[serde(default = "default_warmup_phase1_hours")]
    pub warmup_phase1_hours: u64,
    /// 适应期 RPM 上限(默认 2)。
    #[serde(default = "default_warmup_phase1_rpm")]
    pub warmup_phase1_rpm: u32,
    /// 爬坡期截止(小时):号龄在 [phase1_hours, phase2_hours) 内,有效 RPM =
    /// [`Self::warmup_phase2_rpm`];号龄 ≥ 本值即毕业,恢复账号自身配置。
    #[serde(default = "default_warmup_phase2_hours")]
    pub warmup_phase2_hours: u64,
    /// 爬坡期 RPM 上限(默认 6)。
    #[serde(default = "default_warmup_phase2_rpm")]
    pub warmup_phase2_rpm: u32,
    /// **RPM 闸等待预算**(毫秒):合格号全部仅因 RPM 定频(账号 `extra.rpm_limit` 或暖机
    /// 上限)暂不可选时,等滑动窗口腾出名额再选的最长时长;耗尽仍等不到才报错。
    /// 默认 10000(10s)。
    ///
    /// 背景(2026-08-11 CUR 组生产事故):该等待原先搭 [`Self::queue_wait_ms`] 的车
    /// (排队模式专属预算),而 queue_wait_ms 默认 0、CUR 组没开排队 —— 唯一合格的
    /// cursor 号顶着自己的 rpm_limit 时,请求一秒都不等,直接 503,日志还误报
    /// 「组内所有账号均已禁用」。拆成独立预算后:等待发生在**响应开始之前**,客户端
    /// 只是变慢(yapi 空闲判定 300s,10s 预算远在其内)。
    ///
    /// 预算耗尽后的错误是 **AllRpmLimited**(独立变体,503 与中性客户端文案同
    /// AllBusy):号没死、并发也没满,只是在自我限速 —— 日志与 /health 统计
    /// 不该再撒谎。生效预算钳到 ≤ RPM 窗口(60s),再大是死重。
    #[serde(default = "default_rpm_wait_ms")]
    pub rpm_wait_ms: u64,
    /// **按分组的新号暖机策略**(键 = 分组名):命中分组的低优先新号(rank >= 100、
    /// 号龄 < `hours`)有效 RPM 收紧到 `rpm`;`hours = 0` = 该组**显式关闭暖机**。
    /// 未列出的分组走上面的全局两期暖机(由 `warmup_enabled` 总管)。
    ///
    /// 背景:全局暖机是为 kiro 补货新号设计的(2026-08-10 ha7477062),但它按 rank
    /// 一刀切,把 cursor 订阅号也限到了 2 RPM(2026-08-11 CUR 组 503 事故)。分组策略
    /// 让「哪个组要保护、保护多狠、保护多久」可按业务线分别拍板,互不拖累。
    #[serde(default)]
    pub warmup_group_policies: std::collections::BTreeMap<String, GroupWarmupPolicy>,
}

fn default_rate_limit_cooldown_secs() -> u64 {
    300
}
/// 排队等冷却默认**关闭**(0):新开关不改变既有行为,须显式开启。
fn default_queue_wait_ms() -> u64 {
    0
}
/// 限流节流默认**关闭**(0 = 仍走二值冷却)。
fn default_rate_limit_pace_ms() -> u64 {
    0
}
/// RPM 闸等待预算默认 10s:够一次滑动窗口腾名额,又远在下游空闲判定(yapi 300s)之内。
fn default_rpm_wait_ms() -> u64 {
    10_000
}

/// 一个分组的新号暖机策略(见 [`SchedulerConfig::warmup_group_policies`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupWarmupPolicy {
    /// 新号 RPM 上限(生效期内的有效上限;clamp ≥1,0 = "一次都不许"太反直觉)。
    pub rpm: u32,
    /// 暖机时长(小时):号龄 < 本值才限速。**0 = 该组显式关闭暖机**。
    pub hours: u64,
}
fn default_rate_limit_pace_max_strikes() -> u32 {
    0
}
/// 降层前等待默认**关闭**(0 = 节流即降层,与开关引入前逐字节等价)。
fn default_tier_hold_ms() -> u64 {
    0
}
fn default_tier_hold_window_ms() -> u64 {
    0
}
/// 暖机默认**开启**:补货新号是封号重灾区,默认保护;高优号不受影响,老号(>24h)无感。
fn default_warmup_enabled() -> bool {
    true
}
fn default_warmup_phase1_hours() -> u64 {
    2
}
fn default_warmup_phase1_rpm() -> u32 {
    2
}
fn default_warmup_phase2_hours() -> u64 {
    24
}
fn default_warmup_phase2_rpm() -> u32 {
    6
}
fn default_suspended_cooldown_secs() -> u64 {
    3600
}
fn default_suspended_backoff_cap_secs() -> u64 {
    86400
}
fn default_suspend_retire_strikes() -> u32 {
    3
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
/// 默认 0 = 关闭。开关式引入:不配置时行为与引入前逐字节一致,线上按需热调。
fn default_affinity_hold_ms() -> u64 {
    0
}
/// 默认 true = 保持引入本开关前的跨层向上迁移行为。
fn default_affinity_migrate_up() -> bool {
    true
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            rate_limit_cooldown_secs: default_rate_limit_cooldown_secs(),
            suspended_cooldown_secs: default_suspended_cooldown_secs(),
            suspended_backoff_cap_secs: default_suspended_backoff_cap_secs(),
            suspend_retire_strikes: default_suspend_retire_strikes(),
            empty_response_cooldown_secs: default_empty_response_cooldown_secs(),
            empty_response_window_secs: default_empty_response_window_secs(),
            empty_response_threshold: default_empty_response_threshold(),
            max_failures: default_max_failures(),
            affinity_ttl_secs: default_affinity_ttl_secs(),
            affinity_hold_ms: default_affinity_hold_ms(),
            affinity_migrate_up: default_affinity_migrate_up(),
            quota_poll_enabled: default_quota_poll_enabled(),
            max_switch_attempts: default_max_switch_attempts(),
            queue_wait_ms: default_queue_wait_ms(),
            rate_limit_pace_ms: default_rate_limit_pace_ms(),
            rate_limit_pace_max_strikes: default_rate_limit_pace_max_strikes(),
            tier_hold_ms: default_tier_hold_ms(),
            tier_hold_window_ms: default_tier_hold_window_ms(),
            warmup_enabled: default_warmup_enabled(),
            warmup_phase1_hours: default_warmup_phase1_hours(),
            warmup_phase1_rpm: default_warmup_phase1_rpm(),
            warmup_phase2_hours: default_warmup_phase2_hours(),
            warmup_phase2_rpm: default_warmup_phase2_rpm(),
            rpm_wait_ms: default_rpm_wait_ms(),
            warmup_group_policies: std::collections::BTreeMap::new(),
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
///
/// ⚠️ **这里曾经挂 `#[serde(deny_unknown_fields)]`,2026-07-31 移除。** 原因是它把
/// 「拼错 key 要拒绝」(写侧诉求)和「不认识的 key 要容忍」(读侧诉求)绑成了同一个开关,
/// 而 worker 的 30s 轮询是 `from_str(..).ok().unwrap_or_default()` —— **一个不认识的 key
/// 就让整份 overlay 归零**,静默回落 YAML 基线(`rate_limit_cooldown_secs=300`/`pace=0`),
/// 企业号几秒内被 300s 冷却全部打下线 → 全量 503,而面板读 router 的 effective 照常显示热值。
///
/// 实际后果(实测):`caio-worker-dario` 跑 4 天前的镜像、与主栈共享同一个 SQLite,
/// 自从 DB 里写入 `rate_limit_pace_ms` 起就一直在用空 overlay 跑,**日志里一条告警都没有**。
///
/// 现在改成:**读侧宽容**(未知 key 落进 [`Self::unknown`],其余字段照常生效),
/// **写侧仍然严格**(`admin/settings.rs` 的 PUT 检查 `unknown` 非空即 400,拼错保护不变)。
/// 于是同一个类型能同时满足两侧,且**不需要维护一份会漂移的 KNOWN_KEYS 常量**。
///
/// ⚠️ 本修复保护不了「回滚到 2026-07-31 之前的镜像」—— 那些镜像仍带 `deny_unknown_fields`。
/// 该版本铺满生产后即为**回滚地板**,地板铺满前不许给本结构体新增任何字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// 封禁冷却退避上限(秒)。详见 [`SchedulerConfig::suspended_backoff_cap_secs`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_backoff_cap_secs: Option<u64>,
    /// 连续 suspend 复活失败自动退役阈值(0 = 不退役)。
    /// 详见 [`SchedulerConfig::suspend_retire_strikes`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_retire_strikes: Option<u32>,
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
    /// 排队等冷却的最长等待毫秒(None = 用 yaml 基线;0 = 关闭)。详见
    /// [`SchedulerConfig::queue_wait_ms`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<u64>,
    /// 限流节流间隔(毫秒;None = 用 yaml 基线,0 = 关)。详见 [`SchedulerConfig::rate_limit_pace_ms`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_pace_ms: Option<u64>,
    /// 节流熔断阈值。详见 [`SchedulerConfig::rate_limit_pace_max_strikes`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_pace_max_strikes: Option<u32>,
    /// 降层前为高优先层等待的最长毫秒(None = 用 yaml 基线;0 = 关闭)。
    /// 详见 [`SchedulerConfig::tier_hold_ms`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_hold_ms: Option<u64>,
    /// 请求级等待窗口(毫秒)。详见 [`SchedulerConfig::tier_hold_window_ms`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_hold_window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_ttl_secs: Option<u64>,
    /// 会话亲和粘性预算(毫秒;None = 用 yaml 基线;0 = 关闭)。
    /// 详见 [`SchedulerConfig::affinity_hold_ms`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_hold_ms: Option<u64>,
    /// 已钉扎会话是否允许跨层向上迁移(None = 用 yaml 基线默认 true)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_migrate_up: Option<bool>,
    /// worker 后台配额轮询热开关(None = 用 yaml 基线默认 true)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_poll_enabled: Option<bool>,
    // —— 低优先新号暖机(详见 SchedulerConfig 同名字段;None = 用 yaml 基线)——
    /// 暖机总开关(默认 true)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_enabled: Option<bool>,
    /// 适应期时长(小时,默认 2)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_phase1_hours: Option<u64>,
    /// 适应期 RPM 上限(默认 2)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_phase1_rpm: Option<u32>,
    /// 爬坡期截止(小时,默认 24;号龄 ≥ 此值即毕业)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_phase2_hours: Option<u64>,
    /// 爬坡期 RPM 上限(默认 6)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_phase2_rpm: Option<u32>,
    /// RPM 闸等待预算(毫秒,默认 10000)。详见 [`SchedulerConfig::rpm_wait_ms`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_wait_ms: Option<u64>,
    /// 按分组的新号暖机策略(整图替换;None = 用 yaml 基线)。
    /// 详见 [`SchedulerConfig::warmup_group_policies`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_group_policies:
        Option<std::collections::BTreeMap<String, GroupWarmupPolicy>>,
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
    /// thinking 块是否附 signature(默认开;多上游反代关掉以免 Kiro 合成签名漂到真 Anthropic/Bedrock
    /// 通道被拒)。None = 用基线默认(开)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<bool>,
    /// 主推理上游端点:false=`runtime.kiro.dev`(默认/现状),true=`q.amazonaws.com`(kiro.rs 端点,
    /// 做服务端 prompt 缓存、真实命中 82-92% 省积分)。None=用基线默认。见 [`ExperimentalConfig::q_endpoint`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q_endpoint: Option<bool>,
    // —— thinking ——
    /// 客户端未指定 effort 时的默认思考档位。None = 用基线默认([`DEFAULT_THINKING_EFFORT`])。
    /// 类型是枚举,所以 `PUT /settings` 那步 `from_value::<SystemSettings>` 就会拒掉非法档位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_effort: Option<ThinkingEffort>,
    /// 热追加/覆盖的 cursor 模型目录(整表替换;None = 用 yaml 基线)。
    /// 详见 [`SystemConfig::cursor_extra_models`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_extra_models: Option<Vec<ExtraModelSpec>>,
    /// cursor 内建工具护栏的策略句(None = 用 yaml 基线;空串 = 回 gw-cursor 内置默认)。
    /// 详见 [`SystemConfig::cursor_tool_guard`]。**只影响 cursor 家族。**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_tool_guard: Option<String>,
    /// 兜住本版本**不认识**的 overlay key(新镜像写、旧镜像读的滚动升级窗口)。
    ///
    /// 存在的唯一理由是让「一个陌生 key」不再作废整份 overlay。它有两个消费者:
    /// - 读侧(worker 轮询/启动):非空即告警并列出 key 名,其余字段照常生效;
    /// - 写侧(`PUT /admin/api/settings`):非空即 **400**,取代原先 `deny_unknown_fields`
    ///   提供的拼错保护 —— 因为这个 map 装的就是「所有没被任何字段认领的 key」。
    ///
    /// `skip_serializing_if` 保证 [`Self::from_effective`] 造出来的全量视图不会凭空
    /// 多出一个空对象(admin 的 GET 直接回显它)。
    #[serde(flatten, default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub unknown: std::collections::BTreeMap<String, serde_json::Value>,
}

impl SystemSettings {
    /// 本版本不认识的 overlay key 名(排序、去重后)。空 = 完全认识。
    pub fn unknown_keys(&self) -> Vec<&str> {
        self.unknown.keys().map(String::as_str).collect()
    }
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
        if let Some(v) = self.suspended_backoff_cap_secs { base.scheduler.suspended_backoff_cap_secs = v; }
        if let Some(v) = self.suspend_retire_strikes { base.scheduler.suspend_retire_strikes = v; }
        if let Some(v) = self.empty_response_cooldown_secs { base.scheduler.empty_response_cooldown_secs = v; }
        if let Some(v) = self.empty_response_window_secs { base.scheduler.empty_response_window_secs = v; }
        if let Some(v) = self.empty_response_threshold { base.scheduler.empty_response_threshold = v; }
        if let Some(v) = self.max_failures { base.scheduler.max_failures = v; }
        if let Some(v) = self.max_switch_attempts { base.scheduler.max_switch_attempts = v; }
        if let Some(v) = self.queue_wait_ms { base.scheduler.queue_wait_ms = v; }
        if let Some(v) = self.rate_limit_pace_ms { base.scheduler.rate_limit_pace_ms = v; }
        if let Some(v) = self.rate_limit_pace_max_strikes { base.scheduler.rate_limit_pace_max_strikes = v; }
        if let Some(v) = self.tier_hold_ms { base.scheduler.tier_hold_ms = v; }
        if let Some(v) = self.tier_hold_window_ms { base.scheduler.tier_hold_window_ms = v; }
        if let Some(v) = self.affinity_ttl_secs { base.scheduler.affinity_ttl_secs = v; }
        if let Some(v) = self.affinity_hold_ms { base.scheduler.affinity_hold_ms = v; }
        if let Some(v) = self.affinity_migrate_up { base.scheduler.affinity_migrate_up = v; }
        if let Some(v) = self.quota_poll_enabled { base.scheduler.quota_poll_enabled = v; }
        if let Some(v) = self.warmup_enabled { base.scheduler.warmup_enabled = v; }
        if let Some(v) = self.warmup_phase1_hours { base.scheduler.warmup_phase1_hours = v; }
        if let Some(v) = self.warmup_phase1_rpm { base.scheduler.warmup_phase1_rpm = v; }
        if let Some(v) = self.warmup_phase2_hours { base.scheduler.warmup_phase2_hours = v; }
        if let Some(v) = self.warmup_phase2_rpm { base.scheduler.warmup_phase2_rpm = v; }
        if let Some(v) = self.rpm_wait_ms { base.scheduler.rpm_wait_ms = v; }
        if let Some(v) = &self.warmup_group_policies {
            base.scheduler.warmup_group_policies = v.clone();
        }
        if let Some(v) = self.image_enabled { base.image.enabled = v; }
        if let Some(v) = self.image_max_long_edge { base.image.max_long_edge = v; }
        if let Some(v) = self.image_max_pixels_single { base.image.max_pixels_single = v; }
        if let Some(v) = self.image_max_pixels_multi { base.image.max_pixels_multi = v; }
        if let Some(v) = self.image_multi_threshold { base.image.multi_threshold = v; }
        if let Some(v) = self.tools_in_prefix { base.experimental.tools_in_prefix = v; }
        if let Some(v) = self.cache_point { base.experimental.cache_point = v; }
        if let Some(v) = self.agent_continuation { base.experimental.agent_continuation = v; }
        if let Some(v) = self.thinking_signature { base.experimental.thinking_signature = v; }
        if let Some(v) = self.q_endpoint { base.experimental.q_endpoint = v; }
        if let Some(v) = self.default_thinking_effort { base.thinking.default_effort = v; }
        if let Some(v) = &self.cursor_extra_models {
            base.cursor_extra_models = v.clone();
        }
        if let Some(v) = &self.cursor_tool_guard {
            base.cursor_tool_guard = v.clone();
        }
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
            suspended_backoff_cap_secs: Some(cfg.scheduler.suspended_backoff_cap_secs),
            suspend_retire_strikes: Some(cfg.scheduler.suspend_retire_strikes),
            empty_response_cooldown_secs: Some(cfg.scheduler.empty_response_cooldown_secs),
            empty_response_window_secs: Some(cfg.scheduler.empty_response_window_secs),
            empty_response_threshold: Some(cfg.scheduler.empty_response_threshold),
            max_failures: Some(cfg.scheduler.max_failures),
            max_switch_attempts: Some(cfg.scheduler.max_switch_attempts),
            queue_wait_ms: Some(cfg.scheduler.queue_wait_ms),
            rate_limit_pace_ms: Some(cfg.scheduler.rate_limit_pace_ms),
            rate_limit_pace_max_strikes: Some(cfg.scheduler.rate_limit_pace_max_strikes),
            tier_hold_ms: Some(cfg.scheduler.tier_hold_ms),
            tier_hold_window_ms: Some(cfg.scheduler.tier_hold_window_ms),
            affinity_ttl_secs: Some(cfg.scheduler.affinity_ttl_secs),
            affinity_hold_ms: Some(cfg.scheduler.affinity_hold_ms),
            affinity_migrate_up: Some(cfg.scheduler.affinity_migrate_up),
            quota_poll_enabled: Some(cfg.scheduler.quota_poll_enabled),
            warmup_enabled: Some(cfg.scheduler.warmup_enabled),
            warmup_phase1_hours: Some(cfg.scheduler.warmup_phase1_hours),
            warmup_phase1_rpm: Some(cfg.scheduler.warmup_phase1_rpm),
            warmup_phase2_hours: Some(cfg.scheduler.warmup_phase2_hours),
            warmup_phase2_rpm: Some(cfg.scheduler.warmup_phase2_rpm),
            rpm_wait_ms: Some(cfg.scheduler.rpm_wait_ms),
            warmup_group_policies: Some(cfg.scheduler.warmup_group_policies.clone()),
            image_enabled: Some(cfg.image.enabled),
            image_max_long_edge: Some(cfg.image.max_long_edge),
            image_max_pixels_single: Some(cfg.image.max_pixels_single),
            image_max_pixels_multi: Some(cfg.image.max_pixels_multi),
            image_multi_threshold: Some(cfg.image.multi_threshold),
            tools_in_prefix: Some(cfg.experimental.tools_in_prefix),
            cache_point: Some(cfg.experimental.cache_point),
            agent_continuation: Some(cfg.experimental.agent_continuation),
            thinking_signature: Some(cfg.experimental.thinking_signature),
            q_endpoint: Some(cfg.experimental.q_endpoint),
            default_thinking_effort: Some(cfg.thinking.default_effort),
            cursor_extra_models: Some(cfg.cursor_extra_models.clone()),
            cursor_tool_guard: Some(cfg.cursor_tool_guard.clone()),
            // 全量视图由本进程的有效配置构造,按定义不含未知 key。
            unknown: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 复刻 2026-07-31 的生产事故形状:新镜像往 DB 写了一个 key,旧镜像读到它。
    ///
    /// 事故时 `SystemSettings` 带 `deny_unknown_fields`,worker 又是
    /// `from_str(..).ok().unwrap_or_default()` —— 于是整份 overlay 归零、
    /// `rate_limit_cooldown_secs` 从热值 2 退回 YAML 基线 300,企业号被全部冷却下线。
    /// 实测受害者:`caio-worker-dario`(4 天前的镜像,与主栈共享同一 SQLite),静默跑了几天。
    #[test]
    fn unknown_overlay_key_must_not_void_the_known_ones() {
        let json = r#"{
            "rate_limit_cooldown_secs": 2,
            "queue_wait_ms": 15000,
            "rate_limit_pace_ms": 250,
            "a_knob_from_a_newer_build": 42
        }"#;
        let s: SystemSettings =
            serde_json::from_str(json).expect("未知字段不该让整份 overlay 解析失败");
        assert_eq!(s.unknown_keys(), vec!["a_knob_from_a_newer_build"]);

        let mut base = SystemConfig::default();
        assert_eq!(
            base.scheduler.rate_limit_cooldown_secs,
            default_rate_limit_cooldown_secs(),
            "前提:YAML 基线的冷却远大于热值,所以 overlay 一旦作废就是事故"
        );
        s.apply_to(&mut base);
        assert_eq!(base.scheduler.rate_limit_cooldown_secs, 2, "已知字段必须照常生效");
        assert_eq!(base.scheduler.queue_wait_ms, 15000);
        assert_eq!(base.scheduler.rate_limit_pace_ms, 250);
    }

    /// 未知字段要**原样保留并回显**:admin 的 GET 不能把它们吃掉,
    /// 否则运维在面板上看不出「库里还有本进程不认识的东西」。
    #[test]
    fn unknown_overlay_keys_round_trip() {
        let s: SystemSettings =
            serde_json::from_str(r#"{"max_failures":7,"future_knob":"x"}"#).unwrap();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v.get("future_knob").and_then(|x| x.as_str()), Some("x"));
        assert!(
            v.get("unknown").is_none(),
            "flatten 的兜底 map 不该以 `unknown` 这个名字出现在线缆上"
        );
    }

    /// 全量视图由本进程有效配置构造,按定义不含未知 key —— 不能凭空多出字段。
    #[test]
    fn from_effective_carries_no_unknown_keys() {
        let full = SystemSettings::from_effective(&SystemConfig::default(), None);
        assert!(full.unknown.is_empty());
        let v = serde_json::to_value(&full).unwrap();
        assert!(v.get("unknown").is_none());
    }

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
    fn restock_config_defaults_and_parse() {
        // 缺省:不参与补货、无密钥。这条最重要——默认值必须是"不花钱"。
        let d = RestockConfig::default();
        assert!(!d.enabled);
        assert!(!d.is_configured());
        assert_eq!(d.base_url(), RestockConfig::DEFAULT_BASE_URL);

        // 老 system.yaml 缺 restock 段 → 默认,不拒绝启动(顶层无 deny_unknown_fields)。
        let cfg: SystemConfig = serde_yaml::from_str("upstream_timeout_secs: 600\n").unwrap();
        assert!(!cfg.restock.enabled);

        // enabled 但没密钥 → fail-closed,不算配置完整(否则会带着必然 401 的客户端空转)。
        let half = "restock:\n  enabled: true\n";
        let cfg2: SystemConfig = serde_yaml::from_str(half).unwrap();
        assert!(cfg2.restock.enabled);
        assert!(!cfg2.restock.is_configured());

        // 完整配置。尾斜杠要被去掉,否则拼路径出 `//api/me/stock`。
        let yaml = "restock:\n  enabled: true\n  base_url: \"https://drop.kiro.ss/\"\n  api_key: \"usr-abc\"\n";
        let cfg3: SystemConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg3.restock.is_configured());
        assert_eq!(cfg3.restock.base_url(), "https://drop.kiro.ss");

        // 段内拼错字段当场报错(deny_unknown_fields),不静默忽略。
        assert!(serde_yaml::from_str::<SystemConfig>("restock:\n  enabeld: true\n").is_err());
    }

    #[test]
    fn parse_instances_with_local_ip_and_proxy() {
        let yaml = r#"
router:
  listen: "0.0.0.0:8990"
workers:
  - instance: 0
    listen: "127.0.0.1:9000"
    egress: { mode: local_ip, address: "139.180.152.158" }
    account_group: "G0"
  - instance: 1
    listen: "127.0.0.1:9001"
    egress: { mode: proxy, url: "socks5://127.0.0.1:1080" }
    account_group: "G1"
"#;
        let cfg: InstancesConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.workers.len(), 2);
        match &cfg.worker(0).unwrap().egress {
            EgressConfig::LocalIp { address } => assert_eq!(address, "139.180.152.158"),
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

    #[test]
    fn settings_warmup_apply_to_overrides_and_preserves() {
        // None → 保留基线(默认开,2h/2,24h/6);Some → 覆盖。验证暖机字段的 overlay 语义。
        let mut base = SystemConfig::default();
        SystemSettings::default().apply_to(&mut base);
        assert!(base.scheduler.warmup_enabled, "基线默认开");
        assert_eq!(base.scheduler.warmup_phase1_rpm, 2);
        let s = SystemSettings {
            warmup_enabled: Some(false),
            warmup_phase1_hours: Some(4),
            warmup_phase2_rpm: Some(9),
            ..Default::default()
        };
        s.apply_to(&mut base);
        assert!(!base.scheduler.warmup_enabled, "Some(false) 应关掉暖机");
        assert_eq!(base.scheduler.warmup_phase1_hours, 4);
        assert_eq!(base.scheduler.warmup_phase2_rpm, 9);
        assert_eq!(base.scheduler.warmup_phase1_rpm, 2, "未提供的字段不动");
        assert_eq!(base.scheduler.warmup_phase2_hours, 24, "未提供的字段不动");
    }

    #[test]
    fn settings_thinking_signature_apply_to_overrides_and_preserves() {
        // None → 不覆盖(保留基线默认开);Some(false) → 关签名。验证 overlay 语义对新字段成立。
        let mut base = SystemConfig::default();
        base.experimental.thinking_signature = true;
        SystemSettings::default().apply_to(&mut base);
        assert!(base.experimental.thinking_signature, "None 时应保留默认(不覆盖)");
        let s_off = SystemSettings { thinking_signature: Some(false), ..Default::default() };
        s_off.apply_to(&mut base);
        assert!(!base.experimental.thinking_signature, "Some(false) 应关掉 thinking 签名");
    }

    #[test]
    fn thinking_default_effort_baseline_is_the_shared_constant() {
        // 基线默认必须来自 DEFAULT_THINKING_EFFORT —— gw-kiro 的 DEFAULT_EFFORT 是它的别名,
        // 两处若各写一份字面量,改了一处就会静默漂移。
        assert_eq!(SystemConfig::default().thinking.default_effort, DEFAULT_THINKING_EFFORT);
        assert_eq!(ThinkingConfig::default().default_effort, DEFAULT_THINKING_EFFORT);
        assert_eq!(DEFAULT_THINKING_EFFORT.as_str(), "high");
    }

    #[test]
    fn yaml_without_thinking_section_gets_the_default_effort() {
        // 老的 system.yaml 没有 thinking 段 —— 必须回落默认而不是空串(空串会让 adapter
        // 拿到一个非法档位)。
        let cfg: SystemConfig = serde_yaml::from_str("upstream_timeout_secs: 900\n").unwrap();
        assert_eq!(cfg.thinking.default_effort, DEFAULT_THINKING_EFFORT);
    }

    #[test]
    fn thinking_section_unknown_field_is_rejected() {
        // deny_unknown_fields:yaml 里拼错字段名要当场报错,而不是静默用默认档位。
        let r = serde_yaml::from_str::<SystemConfig>("thinking:\n  defualt_effort: max\n");
        assert!(r.is_err(), "拼错的 thinking 子字段必须被拒绝");
    }

    /// 对抗审查(三个 lens 一致)的根因修复:非法档位必须在**配置装载边界**就不可表示,
    /// 否则控制面显示值与数据面生效值会永久分叉。
    #[test]
    fn illegal_effort_in_yaml_is_rejected_at_load_time() {
        for bad in ["ultra", "hihg", "HIGH", "", "9"] {
            let r = serde_yaml::from_str::<SystemConfig>(&format!(
                "thinking:\n  default_effort: \"{bad}\"\n"
            ));
            assert!(r.is_err(), "非法档位 {bad:?} 必须在装载时被拒,而不是留到消费点");
        }
        // 五个合法档位都要能装载(别把枚举拼错了)。
        for good in ThinkingEffort::ALL {
            let cfg: SystemConfig = serde_yaml::from_str(&format!(
                "thinking:\n  default_effort: {}\n",
                good.as_str()
            ))
            .unwrap_or_else(|e| panic!("合法档位 {} 应能装载: {e}", good.as_str()));
            assert_eq!(cfg.thinking.default_effort, good);
        }
    }

    #[test]
    fn effort_serializes_to_lowercase_wire_form() {
        // 序列化形态即 wire 形态,上游只认小写。前端也按这个串比对/高亮。
        assert_eq!(serde_json::to_string(&ThinkingEffort::Xhigh).unwrap(), "\"xhigh\"");
        for e in ThinkingEffort::ALL {
            assert_eq!(serde_json::to_string(&e).unwrap(), format!("\"{}\"", e.as_str()));
        }
    }

    #[test]
    fn settings_default_thinking_effort_apply_to_overrides_and_preserves() {
        let mut base = SystemConfig::default();
        SystemSettings::default().apply_to(&mut base);
        assert_eq!(
            base.thinking.default_effort, DEFAULT_THINKING_EFFORT,
            "None 时应保留基线默认(不覆盖)"
        );
        let s = SystemSettings {
            default_thinking_effort: Some(ThinkingEffort::Xhigh),
            ..Default::default()
        };
        s.apply_to(&mut base);
        assert_eq!(base.thinking.default_effort, ThinkingEffort::Xhigh, "Some 应覆盖基线");
    }

    #[test]
    fn from_effective_carries_default_thinking_effort() {
        // from_effective 必须回灌该字段:否则 GET /settings 拿不到当前生效档位,
        // 且 worker 30s 轮询会因字段缺失而丢掉这个设置(轮询走的是同一个 from_effective)。
        let mut cfg = SystemConfig::default();
        cfg.thinking.default_effort = ThinkingEffort::Medium;
        let full = SystemSettings::from_effective(&cfg, None);
        assert_eq!(full.default_thinking_effort, Some(ThinkingEffort::Medium));
    }

    #[test]
    fn settings_q_endpoint_default_off_overlay_and_roundtrip() {
        // 无 env 时默认关(runtime.kiro.dev)。
        assert!(!super::default_q_endpoint(), "q_endpoint 默认关(无 KIRO_Q_ENDPOINT)");
        // None → 不覆盖(保留基线);Some(true) → 切旧 q 端点。
        let mut base = SystemConfig::default();
        base.experimental.q_endpoint = false;
        SystemSettings::default().apply_to(&mut base);
        assert!(!base.experimental.q_endpoint, "None 时应保留默认(不误切端点)");
        let s_on = SystemSettings { q_endpoint: Some(true), ..Default::default() };
        s_on.apply_to(&mut base);
        assert!(base.experimental.q_endpoint, "Some(true) 应切到 q.amazonaws.com 端点");
        // from_effective 往返:开着的 q_endpoint 应被 Some(true) 回灌。
        let full = SystemSettings::from_effective(&base, None);
        assert_eq!(full.q_endpoint, Some(true), "from_effective 须回灌 q_endpoint 当前值");
    }

    #[test]
    fn from_effective_carries_thinking_signature() {
        // from_effective 必须回灌 thinking_signature(否则前端拿不到真值 / 轮询热应用丢字段)。
        let mut cfg = SystemConfig::default();
        cfg.experimental.thinking_signature = false;
        let s = SystemSettings::from_effective(&cfg, None);
        assert_eq!(s.thinking_signature, Some(false), "from_effective 应带 thinking_signature 真值");
    }
}
