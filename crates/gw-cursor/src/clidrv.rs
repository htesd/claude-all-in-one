//! CLI 驱动:把 `cursor-agent` 官方 CLI 当上游(子进程模式)。
//!
//! 2026-08-16 决策:Cursor 的服务端持史是「内容寻址存储 + 哈希回显握手」,
//! 线协议合成续轮的代价比预期深(见 PROTOCOL-agent-run.md §20)。兜底方案 =
//! 不模拟协议,直接驱动真客户端 —— 协议保真 100%,多轮/持史/缓存命中全免费。
//!
//! ## 驱动方式(全部实测)
//!
//! - **每号一个 HOME**(`<base>/<account_id>/`):把号库里的 access/refresh token
//!   写成 `.config/cursor/auth.json` 即完成登录,CLI 自己刷新并回写。
//! - **每次请求 spawn 一次** `-p --output-format stream-json --stream-partial-output`;
//!   首轮新会话,从 `system.init` 事件拿 `session_id`;后续轮 `--resume <id>`。
//! - **安全闸**:`--mode ask`(read-only)且**绝不**加 `--force`。注意 ask 模式
//!   仍允许**只读 shell**(ls/cat 会真跑)—— 部署侧用**每账号独立 uid** + 700
//!   HOME 隔离:别号的 CLI 连目录都进不来,auth.json 互不可读(见 `account_uid`)。
//! - 调用方 system 提示写进**每会话**工作区(`<home>/ws/<conversation_id>/`)的
//!   `AGENTS.md`(CLI 原生 rules 位置)。**每号一份是错的**:AGENTS.md 装的是调用方
//!   本次请求的 system,同号并发的两个会话会互相覆盖它,还会让甲客户的提示被乙客户的
//!   CLI 读到(2026-08-17 线上实测,见 [`prepare_home`])。
//! - **工具桥**(调用方声明了 tools 时):每号 HOME 写死 `.cursor/mcp.json` +
//!   `permissions.json`(`{"mcpAllowlist":["gwtools:*"]}`,格式挖自 CLI bundle 的
//!   `shouldBlockMcp`/`matchesMcpPattern`,必须带冒号)。桥是本网关二进制的
//!   `--mode cursor-mcp-bridge` 子进程;CLI 调工具 → 桥挂起 → 网关发 tool_use
//!   给客户 → 客户下次请求带 tool_result → 桥应答 → CLI 继续。
//! - **图片**:落到每会话工作区的 `assets/`,提示词带绝对路径(ask 模式只读工具
//!   能读图,实测左红右蓝识别正确);**PDF**:抽文本层内联进提示词。
//!
//! ## 事件流(stream-json,逐行 NDJSON)
//!
//! `system.init`(session_id)→ `thinking.delta`×N → `assistant` 增量×N
//! → `assistant` 全量回显(**无 timestamp_ms**,要去重)→ `result`(usage 是
//! **上游真实值**,含 cacheReadTokens)。
//!
//! ## 跨请求接力(工具回路的关键)
//!
//! 反代是一问一答,CLI 是长会话。桥接期间 CLI 进程**保持存活**:泵任务把
//! 「阶段输出」写进 [`OutQueue`],每个 Anthropic 响应是一条 drain 流;泵在
//! 桥调用处挂起,等下一轮请求把 tool_result 喂进来再继续泵。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, Notify};

use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{ChatUsage, SseEvent, StreamItem};

/// CLI 进程的**活跃时间**上限(不含桥挂起等待)。
///
/// ⚠️ 注意它量的**不是**墙上时钟。检查点在泵的 `select!` 的 500ms tick 分支里,而桥调用
/// 挂起是在 `call` 分支**内部** `await` 一个 [`PENDING_TTL`] 的 timeout —— 那段 await 期间
/// 整个 select 停转、tick 不响,所以挂起等待**不计入**这个预算。
///
/// 这不是 bug 而是必要的:一个 CLI 进程横跨调用方的多个 HTTP 回合(每次桥调用都要
/// 等调用方在自己机器上执行完再发下一个请求),10 轮工具往返的墙上时钟轻易超过 240s。
/// 若把挂起等待也计进来,正常的长工具回路会被误杀。
///
/// 早先这里的注释写的是「单轮 CLI 调用的硬上限……保证先于我方上层超时干净地杀掉进程」,
/// 那句话是错的:它既不是"单轮",也**兜不住**调用方那一侧的等待 —— 客户可见的空等由
/// [`DRAIN_IDLE_TIMEOUT`] 负责。2026-08-17 排 300s 空等时查清。
const CLI_TIMEOUT: Duration = Duration::from_secs(240);
/// 桥调用等调用方带回 tool_result 的上限。超时给桥回错误,让 CLI 干净收尾。
const PENDING_TTL: Duration = Duration::from_secs(280);
/// **调用方一侧**的空闲上限:本次 HTTP 响应连续多久拿不到任何事件就判本轮废了。
///
/// 不设它的后果(2026-08-17 生产实测):`resume_conv` 把 tool_result 喂进挂起槽后直接返回
/// [`drain_stream`],而那个流**没有任何超时** —— CLI 之后若一声不出,调用方就一路等到
/// gw-app 的 300s `STREAM_IDLE_ABORT` 才收到一个空响应。实测占 tool_result 接续请求的
/// 1%~7%,客户看到的是**整整 5 分钟没有任何输出**。
///
/// 取 90s 与线协议的 `chat::STALL_TIMEOUT` 同值(那边的理由:心跳 10s 一个,容忍 9 个)。
/// 这里 OutQueue 里没有心跳,思考增量就是进展信号 —— 连续 90s 一个字节都没有等于死了。
/// 宁可偏大:误判的代价是把一次本来会成功的慢请求变成失败。
const DRAIN_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// 工具轮次估算用量的**校正系数**(乘在 [`crate::chat::est_text_tokens`] 的结果上)。
///
/// 为什么需要它:CLI 只在整个会话结束时发一次 `result` 事件带真实用量(2026-08-17 抓原始
/// ndjson 确认:`thinking`/`assistant` 事件里一个 token 字段都没有)。而工具回路里每一轮
/// 都是调用方独立的一次 HTTP 请求,发 `tool_use` 那一刻上游还没给数 —— 只能自估。
///
/// 系数怎么来的:拿 901 条**有上游真值**的 `end_turn` 请求反标定(近 8 小时,team4/5/6 +
/// ultra-test)。`est_text_tokens(可见正文 + thinking + tool_use 参数)` 只有上游真值的
/// **0.54 倍**(中位 0.58,P5=0.17/P95=0.97),总量对齐需要 1.85。
///
/// 差额的来源是**隐藏推理**:流里的 `thinking` 是摘要,上游按完整 CoT 计费 —— 这与
/// 「加密 CoT 体积 ≈ 摘要的 2.3 倍」那次测量同向(见记忆 caio-thinking-blob-extraction)。
///
/// ⚠️ 这是**估算**,只保证总量口径对得上,单条请求可能偏 2 倍以上(见上面的 P5/P95)。
/// 取值偏保守一侧:1.85 是总量对齐值而非 P75,宁可少收也不多收。
/// 重新标定的脚本口径:按 `stop_reason='end_turn'` 且自身正文 ≥50 字符的样本算总量比。
const TOOL_ROUND_TOKEN_FACTOR: f64 = 1.85;
/// 会话条目 TTL:超时按新会话处理(与线协议形态同值)。
const SESSION_TTL: Duration = Duration::from_secs(2 * 3600);
/// 每会话工作区的保留期。与 [`SESSION_TTL`] 同值:会话条目过期后那个目录再没人会用。
const WS_TTL: Duration = Duration::from_secs(2 * 3600);

/// 解析好的 CLI 二进制与数据根。
#[derive(Debug, Clone)]
pub struct CliDriverConfig {
    /// cursor-agent 可执行文件路径。
    pub bin: PathBuf,
    /// 每号 HOME 的根目录(`<base>/<account_id>/`)。
    pub base_dir: PathBuf,
    /// 本网关二进制(桥子进程用 `--mode cursor-mcp-bridge` 起它)。
    pub self_exe: PathBuf,
}

impl CliDriverConfig {
    pub fn from_env() -> Self {
        let bin = std::env::var("CURSOR_AGENT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/opt/cursor-agent/cursor-agent"));
        let base_dir = std::env::var("CURSOR_CLI_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/app/data/cursor-cli"));
        let self_exe = std::env::var("CURSOR_SELF_BIN")
            .map(PathBuf::from)
            .ok()
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("claude-all-in-one"));
        Self {
            bin,
            base_dir,
            self_exe,
        }
    }
}

// ── token 轮换捕获(CLI 是号库凭据的第二个写者)─────────────────────────────

/// CLI 子进程自刷新轮换出的新凭据,等 worker 周期任务 CAS 落库。
#[derive(Debug, Clone)]
pub struct TokenUpdate {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// `YYYY-MM-DDTHH:MM:SSZ`(与 refresh_auth 回写同格式,字典序可比)。
    pub expires_at: Option<String>,
}

impl TokenUpdate {
    /// 转 accounts.extra 增量字段(键与 refresh_auth 回写口径一致)。
    pub fn to_delta(&self) -> BTreeMap<String, Value> {
        let mut d = BTreeMap::new();
        d.insert("access_token".to_string(), Value::String(self.access_token.clone()));
        if let Some(rt) = &self.refresh_token {
            d.insert("refresh_token".to_string(), Value::String(rt.clone()));
        }
        if let Some(exp) = &self.expires_at {
            d.insert("expires_at".to_string(), Value::String(exp.clone()));
        }
        d
    }
}

/// account_id → 捕获到的轮换凭据。provider 持有;prepare_home / pump 上报,
/// worker 周期任务经 `Provider::poll_token_updates` 取空。
pub type TokenUpdates = Arc<Mutex<HashMap<String, TokenUpdate>>>;

/// 记录一次轮换捕获(同号重复捕获只留最新一份)。
fn report_token_update(
    updates: &TokenUpdates,
    account_id: &str,
    access: &str,
    refresh: Option<&str>,
    exp: Option<i64>,
) {
    updates
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            account_id.to_string(),
            TokenUpdate {
                access_token: access.to_string(),
                refresh_token: refresh.map(str::to_string),
                expires_at: exp.map(crate::auth::format_unix_utc),
            },
        );
    tracing::info!(account = %account_id, "cursor-cli:捕获到 CLI 自刷新轮换的 token,待落库");
}

/// 读 auth.json 的 (accessToken, refreshToken);读不到/缺 accessToken 返回 None。
fn read_auth_creds(path: &Path) -> Option<(String, Option<String>)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let at = v.get("accessToken").and_then(|x| x.as_str())?.to_string();
    let rt = v
        .get("refreshToken")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Some((at, rt))
}

/// 原子写 auth.json(tmp + rename,0600)。
fn write_auth_json(home: &Path, access_token: &str, refresh_token: Option<&str>) -> std::io::Result<()> {
    let rt = refresh_token.unwrap_or(access_token);
    let body = json!({"accessToken": access_token, "refreshToken": rt});
    let tmp = home.join(".config/cursor/.auth.json.tmp");
    std::fs::write(&tmp, body.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, home.join(".config/cursor/auth.json"))
}

// ── 每账号独立 uid(隔离边界的最小单位)──────────────────────────────────────

/// 派生本账号的降权 uid(FNV-1a,跨进程稳定)。
///
/// 所有号的 CLI 共用 nobody 时,同 uid 下 700/600 **不构成边界**:A 号的 CLI 被
/// prompt 注入后 `cat` 就走 B 号的 auth.json(对抗审查共识 S0-1)。每号一个 uid
/// 后,配合 700 HOME,别号进程连目录都不可达。落在 100_000..500_000:不撞
/// 系统/容器 uid,也不需要 /etc/passwd 条目(setuid 只认数字)。
pub fn account_uid(account_id: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in account_id.as_bytes() {
        h = (h ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    100_000 + h % 400_000
}

// ── 每号 HOME ────────────────────────────────────────────────────────────────

/// 备好一个账号的 HOME:auth.json 对账(见下)、MCP 权限白名单、工作区。
///
/// auth.json 有**两个写者**:CLI 自刷新回写它,gw-app OAuth 刷新写号库。
/// 两边都以 JWT exp 论新旧:
/// - 文件新(CLI 轮换过)→ 上报捕获表 `updates`(不落库的话号库里是已作废的
///   旧 refresh_token,下次 gw-app 刷新即 invalid_grant,号砖);
/// - 号库新(gw-app 刷新/人工重录)→ 覆写 auth.json 让 CLI 跟上;
/// - 都解不出 exp → 信文件(CLI 的活状态),不动。
///
/// `system` 非空时同步到工作区 AGENTS.md(内容没变就不写,免得每轮刷 mtime)。
/// 当前 unix 秒。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 回收过期的每会话工作区。尽力而为,任何错误一律忽略(它不是请求成功的前置条件)。
///
/// **判据取显式写下的 `.last` 而不是目录 mtime**:目录 mtime 只在增删目录项时变,一个
/// 已经跑了半小时、期间只读文件的活会话,其目录 mtime 可能很旧 —— 拿它当判据会删掉
/// 正在用的工作区(而 CLI 的 cwd 被删掉之后的行为是另一场排查)。
///
/// 没有 `.last` 的目录一律当过期:那只可能是旧版留下的(如老的 `ws/assets`),或写标记
/// 失败的残骸,两者都没人会再用。当前会话的目录用**路径**排除,不靠名字比较。
fn gc_workspaces(ws_root: &Path, keep: &Path) {
    let Ok(rd) = std::fs::read_dir(ws_root) else {
        return;
    };
    let now = now_secs();
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() || p == keep {
            continue;
        }
        let alive = std::fs::read_to_string(p.join(".last"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|t| now.saturating_sub(t) < WS_TTL.as_secs());
        if !alive {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

pub fn prepare_home(
    cfg: &CliDriverConfig,
    account_id: &str,
    conversation_id: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    system: &str,
    updates: &TokenUpdates,
) -> Result<(PathBuf, PathBuf), UpstreamError> {
    let home = cfg.base_dir.join(account_id);
    let ws_root = home.join("ws");
    // 工作区**每会话一份**。早先是每号一份(`home/ws`),而 AGENTS.md 装的是**调用方本次
    // 请求的 system 提示** —— 同一个号上并发的两个会话会互相覆盖这个文件:
    // 2026-08-17 线上实测,一个无工具的角色扮演会话把它覆成「你没有任何工具可用」+ 人设,
    // 而同号另一个带 46 个工具的 Claude Code 会话的 CLI 读到的就是那份,模型的 thinking 里
    // 写着「gwtools 未直接列入 MCP 列表」。除了功能错乱,这还是**跨会话的提示串味**:
    // 甲客户的 system 提示躺在乙客户的 CLI 工作区里。
    // 目录名用 conversation_id(已是 UUID 形态,见 chat::conversation_uuid,不含路径分隔符)。
    let ws = if conversation_id.is_empty() {
        ws_root.join("_noconv")
    } else {
        ws_root.join(conversation_id)
    };
    let io = |what: &str, e: std::io::Error| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("cursor-cli: {what}失败: {e}"),
        )
    };
    std::fs::create_dir_all(home.join(".config/cursor")).map_err(|e| io("创建 home", e))?;
    std::fs::create_dir_all(home.join(".cursor")).map_err(|e| io("创建 .cursor", e))?;
    std::fs::create_dir_all(ws.join("assets")).map_err(|e| io("创建工作区", e))?;

    // 迁移:旧版把 AGENTS.md 直接写在 `home/ws` 下,而那正是新工作区的**父目录** ——
    // Cursor 会往上层找 rules,留着它等于把串味问题原地保留。见到就删。
    let legacy_agents = ws_root.join("AGENTS.md");
    if legacy_agents.is_file() {
        let _ = std::fs::remove_file(&legacy_agents);
        tracing::info!(account = %account_id, "cursor-cli:清掉旧的每号共享 AGENTS.md(已改每会话)");
    }
    // 活跃标记 + 过期目录回收(判据见 gc_workspaces)。
    let _ = std::fs::write(ws.join(".last"), now_secs().to_string());
    gc_workspaces(&ws_root, &ws);

    let auth_file = home.join(".config/cursor/auth.json");
    if auth_file.exists() {
        if let Some((fat, frt)) = read_auth_creds(&auth_file) {
            if fat != access_token {
                match (
                    crate::auth::token_expires_at(&fat),
                    crate::auth::token_expires_at(access_token),
                ) {
                    // 文件明显更新(或号库的解不出、文件的能解)→ CLI 轮换过,捕获。
                    (Some(fexp), Some(aexp)) if fexp > aexp => {
                        report_token_update(updates, account_id, &fat, frt.as_deref(), Some(fexp));
                    }
                    (Some(fexp), None) => {
                        report_token_update(updates, account_id, &fat, frt.as_deref(), Some(fexp));
                    }
                    // 号库更新 → 覆写文件(旧 refresh_token 可能已作废,CLI 要用新的)。
                    (Some(_), Some(_)) => {
                        write_auth_json(&home, access_token, refresh_token)
                            .map_err(|e| io("覆写 auth.json", e))?;
                    }
                    // 都解不出 exp:信文件,不动。
                    _ => {}
                }
            }
        }
    } else {
        write_auth_json(&home, access_token, refresh_token).map_err(|e| io("落 auth.json", e))?;
    }

    // MCP 工具白名单(格式实证:Mcp(server:tool),冒号必填,支持 glob)。
    let perms = home.join(".cursor/permissions.json");
    let perms_body = r#"{"mcpAllowlist":["gwtools:*"]}"#;
    if std::fs::read_to_string(&perms).unwrap_or_default() != perms_body {
        std::fs::write(&perms, perms_body).map_err(|e| io("写 permissions.json", e))?;
    }

    if !system.is_empty() {
        let agents = ws.join("AGENTS.md");
        if std::fs::read_to_string(&agents).unwrap_or_default() != system {
            std::fs::write(&agents, system).map_err(|e| io("写 AGENTS.md", e))?;
        }
    }

    // 降权隔离:CLI 子进程以**每账号独立 uid** 运行(见 start_conv / account_uid),
    // HOME 归它且 700 —— 别号的 CLI(不同 uid)连目录都进不来,auth.json 互不可读。
    // /app/data 等敏感目录对这些 uid 同样不可读(部署要求 data/ 700、control.db 600)。
    // 非 root 环境(本机开发)chown 会失败,忽略即可。
    #[cfg(unix)]
    {
        let uid = account_uid(account_id);
        let _ = std::process::Command::new("chown")
            .arg("-R")
            .arg(format!("{uid}:{uid}"))
            .arg(&home)
            .status();
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    }
    Ok((home, ws))
}

/// 对外模型名 → CLI 模型名(`--model` 参数)。
/// 一律非 fast 档:fast 变体走加急计费,成本高一截(2026-08-17 用户拍板);
/// 后续要 fast 就单列 `xxx-fast` 对外型号再映射回来。
pub fn cli_model_name(cursor_model: &str) -> String {
    match cursor_model {
        "default" => "auto".into(),
        "grok-4.6" => "cursor-grok-4.6-high".into(),
        "grok-4.5" => "cursor-grok-4.5-high".into(),
        "composer-2.5" => "composer-2.5".into(),
        "claude-opus-5" => "claude-opus-5-thinking-high".into(),
        "claude-sonnet-5" => "claude-sonnet-5-thinking-high".into(),
        "claude-fable-5" => "claude-fable-5-thinking-high".into(),
        "kimi-k3" => "kimi-k3-high".into(),
        other => other.to_string(),
    }
}

// ── NDJSON 事件解析 ─────────────────────────────────────────────────────────

#[derive(Default)]
struct NdjsonState {
    seen_text: String,
    session_id: Option<String>,
    result: Option<Value>,
}

enum Ev {
    Delta(String),
    Thinking(String),
    Done,
    Nothing,
}

fn handle_line(state: &mut NdjsonState, line: &str) -> Ev {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Ev::Nothing;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("system") => {
            if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                state.session_id = Some(sid.to_string());
            }
            Ev::Nothing
        }
        Some("thinking") => {
            if v.get("subtype").and_then(|s| s.as_str()) == Some("delta") {
                if let Some(t) = v.get("text").and_then(|s| s.as_str()) {
                    return Ev::Thinking(t.to_string());
                }
            }
            Ev::Nothing
        }
        Some("assistant") => {
            let text: String = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|bs| {
                    bs.iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                b.get("text").and_then(|t| t.as_str())
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            if text.is_empty() {
                return Ev::Nothing;
            }
            // 事件三分(实测字段差异):
            // - 增量:有 timestamp_ms、无 model_call_id;
            // - 每条 model call 完结快照:有 model_call_id(内容已被增量覆盖,跳过防复读);
            // - 最终全量回显:两者皆无,只补差量。
            if v.get("model_call_id").is_some() {
                return Ev::Nothing;
            }
            if v.get("timestamp_ms").is_some() {
                state.seen_text.push_str(&text);
                Ev::Delta(text)
            } else if text.starts_with(&state.seen_text) {
                // 无时间戳 = 累积快照(每则 assistant 消息完结时一份):只补差量。
                let rest = text[state.seen_text.len()..].to_string();
                state.seen_text = text;
                if rest.is_empty() {
                    Ev::Nothing
                } else {
                    Ev::Delta(rest)
                }
            } else {
                // 快照与已发对不上(工具回路里几乎必现:最终回显只含末段文本)。
                // 丢掉是正确动作(防复读),不构成异常,debug 级即可。
                tracing::debug!("cursor-cli:全量回显与已发增量对不上,忽略该事件");
                Ev::Nothing
            }
        }
        Some("result") => {
            state.result = Some(v);
            Ev::Done
        }
        _ => Ev::Nothing,
    }
}

/// 当前进程是否 root(读 /proc,不引 libc)。
#[cfg(unix)]
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|v| v == "0"))
        })
        .unwrap_or(false)
}

// ── 会话与输出队列 ──────────────────────────────────────────────────────────

enum OutItem {
    Item(Result<StreamItem, UpstreamError>),
    /// 阶段结束:drain 流收到它就收尾(本次 Anthropic 响应完结)。
    End,
}

/// 泵 → 响应流的单向队列。空时 drain 等在 notify 上。
struct OutQueue {
    q: Mutex<VecDeque<OutItem>>,
    notify: Notify,
}

impl OutQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            q: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        })
    }
    fn push(&self, item: OutItem) {
        self.q.lock().unwrap_or_else(|p| p.into_inner()).push_back(item);
        self.notify.notify_one();
    }
    async fn pop(&self) -> OutItem {
        loop {
            if let Some(it) = self.q.lock().unwrap_or_else(|p| p.into_inner()).pop_front() {
                return it;
            }
            self.notify.notified().await;
        }
    }
}

/// 一次调用方请求的**模拟缓存槽**:peek 出来的计费值 + commit 所需的凭据。
///
/// 为什么要按「请求」而不是「会话」存:工具回路里 CLI 进程全程存活,但调用方那侧
/// 每一轮都是**独立的一次 HTTP 请求**,各自带自己的历史、各自 peek。泵在阶段
/// 切换处读这个槽,所以它必须由每轮入口(`start_conv` / `resume_conv`)现填。
///
/// commit 语义与 `chat.rs` 线协议侧逐条一致:**成功交付才提交**(失败轮上游没落下
/// 这一轮,提交会让下一轮凭空拿到折扣),带代际 CAS 防同会话并发乱序覆盖。
pub struct SimSlot {
    /// `account_id \x1f conversation_id`(与线协议同键)。
    pub key: String,
    pub model: String,
    pub fps: Vec<crate::cache_sim::MsgFingerprint>,
    /// peek 时读到的代际号,commit 时做 compare-and-set。
    pub gen: u64,
    /// 本轮上报的 cache_read(**已过三参数夹限**,不是模拟器原始命中量)。
    pub cache_read: u64,
    /// 模拟器的**原始**命中量(未经夹限)。只用于标定观测,不参与计费。
    ///
    /// 标定要的是"模拟器自己估了多少"与上游真值的比值,夹限后的数已经掺进了
    /// 运营参数(cap/floor/mult),拿它比会把定价决策当成估算误差。
    pub raw_hit: u64,
    /// 本轮**上下文总量**(system + tools + 全部历史,模拟器同口径估算)。
    ///
    /// ⚠️ 这是 `input_tokens` 的正确基准,而不是"本轮新增"。理由:Anthropic 语义下
    /// 线缆侧发的是 `input − cache_read`,所以 `input` 必须是**总输入**;若拿
    /// `fresh_in + cache_read` 当 input,则 `uncached` 恒等于 `fresh_in`,
    /// 夹限再怎么调都改不了客户看到的数(2026-08-17 实测客户侧输入只剩 18 token)。
    /// kiro 用的就是总上下文(`final_input_tokens` = tokenUsage/contextUsage/sim_total),
    /// 两条通道必须同口径,客户账单才可比。
    pub sim_total: u64,
}

impl SimSlot {
    /// 提交本轮指纹(消费自身),让下一轮能命中它。**仅在本轮成功交付后调用**。
    fn commit(self) {
        crate::cache_sim::commit(&self.key, &self.model, self.fps, self.gen);
    }
}

/// **未 peek** 的模拟缓存材料。
///
/// 为什么要把「材料」与「已 peek 的槽」分成两个类型:peek 会读出代际号,而代际号
/// 从读出到 commit 之间越久越容易被同会话的别人推进(CAS 失配)。更要紧的是
/// `resume_conv` 那条路 —— 必须**先校验 tool_result 匹配、再 peek**:校验失败的
/// 请求根本不算一次有效轮次,让它先把状态读出来只会拉长竞态窗口
/// (对抗评审 high#3)。所以入口只准备材料,peek 由真正要用的那一步做。
pub struct SimRequest {
    /// `account_id \x1f conversation_id`(与线协议同键)。
    pub key: String,
    pub model: String,
    pub fps: Vec<crate::cache_sim::MsgFingerprint>,
}

impl SimRequest {
    /// 现在 peek:读模拟表拿本轮计费值与代际号。
    ///
    /// ⚠️ 拿到的**原始命中量**要先过 [`crate::cache_sim::reported_cache_read`] 的
    /// 三参数夹限(与 kiro 同口径)才能当上报值 —— 直接用原始值会出现「几乎全命中、
    /// 客户侧输入只剩几十个 token」(2026-08-17 实测 54% 的记录 cache/input > 0.95,
    /// 最高 0.9998)。`cap_ratio` 就是为杜绝这个而存在的。
    pub fn peek(self) -> SimSlot {
        let (sim, gen) = crate::cache_sim::peek(&self.key, &self.model, &self.fps);
        // 上报基准 = 本轮上下文总量(与 `hit` 同出模拟器 tokenizer,比例才有意义)。
        let sim_total = sim.total_tokens as u64;
        let raw_hit = sim.cache_read_tokens as u64;
        let cache_read = crate::cache_sim::reported_cache_read(
            sim_total,
            raw_hit,
            sim_total,
            crate::cache_sim::billing(),
        );
        SimSlot {
            raw_hit,
            key: self.key,
            model: self.model,
            fps: self.fps,
            gen,
            cache_read,
            sim_total,
        }
    }
}

/// 待调用方应答的桥调用。
struct PendingSlot {
    /// 给调用方的 tool_use.id —— 消费槽位时**按它键控**(防错配/重放注入)。
    tool_use_id: String,
    /// 把结果**和下一阶段的模拟槽**一起还给泵任务。
    ///
    /// ⚠️ 模拟槽走这条通道而不是会话上的共享字段,是刻意的所有权设计:
    /// 一个 `SimSlot` 描述的是**某一次调用方 HTTP 请求**,而 `CliConv` 是**会话**级、
    /// 同 `conversation_id` 的并发请求共享它。放在会话上的单槽会被后来者覆盖,
    /// 于是 A 的账单用到 B 的命中数、甚至 A 的 commit 提交了 B 的指纹
    /// (对抗评审 high#2:代际 CAS 只保护表的一致性,保护不了"谁的账单用了谁的槽")。
    /// 顺着这条通道传,槽的所有权就跟着"哪一轮把结果喂回来"走,结构上不可能错配。
    responder: oneshot::Sender<(Result<String, String>, Option<SimSlot>)>,
}

/// 一条存活中的 CLI 会话(可能正挂在桥调用上)。
pub struct CliConv {
    pub account_id: String,
    /// CLI 的 session_id(pump 拿到 init 事件后填)。
    cli_session_id: Mutex<Option<String>>,
    /// 调用方历史逐轮指纹(分叉检测,同线协议形态)。
    pub fps: Mutex<Vec<u64>>,
    out: Arc<OutQueue>,
    pending: Mutex<Option<PendingSlot>>,
    at: Mutex<Instant>,
    /// CLI 子进程的**进程组 id**(= 它自己的 pid,见 spawn 处的 `process_group(0)`)。
    ///
    /// 为什么要记进程组而不是 pid:`cursor-agent` 会自己再拉起
    /// `node index.js worker-server` 帮工。SIGKILL **不传播**,只杀主进程会把帮工留下,
    /// 而 worker 在容器里是 **PID 1**(`/proc/<pid>/status` 的 `NSpid: <host> 1` 实测),
    /// 孤儿于是重挂到 worker 名下 —— 而 worker 是 tokio 进程,只 wait 自己的 `Child`
    /// 句柄,**不会去收养/回收陌生孤儿**。2026-08-17 现场:33 个孤儿 `worker-server`
    /// 各占 ~163MB ≈ 5.4GB。杀整组才收得干净。
    pgid: Mutex<Option<u32>>,
}

impl CliConv {
    pub fn session_id(&self) -> Option<String> {
        self.cli_session_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn has_pending(&self) -> bool {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }
    /// 消费待应答槽:**仅当** `results` 里有与挂起 tool_use_id 匹配的结果。
    ///
    /// 不匹配 → Err 且**保留**槽位:错配/重放的 tool_result 注入进 CLI 是静默
    /// 语义损坏(对抗审查共识 S1-7),宁可显式报错;调用方可带正确结果重试,
    /// 槽位有 PENDING_TTL 兜底。整个「校验 + 消费」在同一把锁里,无竞态窗口。
    fn take_pending_matching(
        &self,
        results: &[(String, String)],
    ) -> Result<(PendingSlot, String), UpstreamError> {
        let mut p = self.pending.lock().unwrap_or_else(|po| po.into_inner());
        let Some(slot) = p.as_ref() else {
            return Err(UpstreamError::bad_request(
                "cursor-cli: 该会话没有等待结果的桥调用(可能已超时或重铺)",
            ));
        };
        match results.iter().find(|(id, _)| *id == slot.tool_use_id) {
            Some((_, text)) => {
                let text = text.clone();
                let slot = p.take().expect("锁内刚确认槽位存在");
                Ok((slot, text))
            }
            None => {
                // 这条以前只回给客户、**不落日志** —— 于是生产上 grep 6 小时日志 0 命中,
                // 只能从 request_logs 的 400/BadRequest 反查(2026-08-17 客户报障时踩到)。
                // 带上双方 id:实测客户会带回自己框架生成的 id(形如 `call-<uuid>-0`),
                // 与我方 `toolu_<32hex>` 天然对不上,不打出来根本判不出是谁的问题。
                tracing::warn!(
                    account = %self.account_id,
                    pending = %slot.tool_use_id,
                    brought = ?results.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                    "cursor-cli:带回的 tool_result 与挂起的 tool_use 不匹配,已拒绝"
                );
                Err(UpstreamError::bad_request_visible(format!(
                    "cursor-cli: 带回的 tool_result 与挂起的 tool_use({})不匹配,已拒绝(可带正确结果重试)",
                    slot.tool_use_id
                )))
            }
        }
    }
    fn touch(&self) {
        *self.at.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }
    fn fresh(&self) -> bool {
        self.at.lock().unwrap_or_else(|p| p.into_inner()).elapsed() < SESSION_TTL
    }
    /// 杀掉这条会话的**整个进程组**(CLI 主进程 + 它自己拉起的 node 帮工)。
    ///
    /// ⚠️ 这个函数以前是**空壳**(`fn kill_procs(&self) {}`,注释写着"所有权在 pump 里,
    /// kill_on_drop 兜底")。那个假设是错的,而且错法是**循环等待**:
    ///
    /// 1. 泵因为「不是 CLI 自己退出」的原因 break(`CLI_TIMEOUT` 240s、桥连接中断);
    /// 2. 调 `kill_procs()` —— 空壳,什么都没做,子进程还活着;
    /// 3. 紧接着 `stderr_task.await`:那个 task 在读子进程 stderr **等 EOF**,
    ///    而子进程活着 → 永远不 EOF → **await 永久阻塞**;
    /// 4. 泵任务因此永不返回 → `PumpArgs.cli`(`Child`)永不 drop →
    ///    `kill_on_drop(true)` **永不触发**;
    /// 5. 子进程只有被杀才会死,而唯一的杀手正卡在等它 —— 死锁。
    ///
    /// 2026-08-17 现场取证:泄漏进程的 stderr 读端全部仍被 worker 持有(= 卡在第 3 步),
    /// 6 小时内「会 break 但 CLI 不自退」的事件 19 次(12 次 CLI_TIMEOUT + 7 次桥中断),
    /// 与当时 >600s 的泄漏进程数 13 个同量级;74 个残留进程吃掉 ~13GB。
    ///
    /// 用 `kill` 命令而不引 libc:与本文件里 `chown` 的既有做法一致(见 `account_uid`
    /// 附近),`kill -KILL -- -<pgid>` 的负号参数就是"整组"。
    fn kill_procs(&self) {
        let Some(pgid) = *self.pgid.lock().unwrap_or_else(|p| p.into_inner()) else {
            return;
        };
        let ok = std::process::Command::new("kill")
            .arg("-KILL")
            .arg("--")
            .arg(format!("-{pgid}"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            // 组已经全退时 kill 会报"no such process",属正常,debug 级别。
            tracing::debug!(pgid, account = %self.account_id, "cursor-cli:杀进程组无对象(可能已自行退出)");
        }
    }
}

/// 会话表:我方 conversation_id → CLI 会话。
#[derive(Default)]
pub struct CliConversations {
    inner: Mutex<HashMap<String, Arc<CliConv>>>,
}

/// 本轮的会话开法。
pub enum CliLookup {
    Fresh,
    Resume(String),
}

impl CliConversations {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<CliConv>>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, conv: &str) -> Option<Arc<CliConv>> {
        self.map().get(conv).cloned()
    }

    pub fn insert(&self, conv: &str, entry: Arc<CliConv>) {
        let mut map = self.map();
        map.insert(conv.to_string(), entry);
        map.retain(|_, e| e.fresh());
    }

    #[allow(dead_code)] // 预留:后台清理任务用
    pub fn remove(&self, conv: &str) {
        if let Some(e) = self.map().remove(conv) {
            e.kill_procs();
        }
    }

    /// 判定本轮怎么开会话(指纹前缀校验,语义同线协议形态)。
    pub fn lookup(&self, conv: &str, account_id: &str, history_fps: &[u64]) -> CliLookup {
        let map = self.map();
        match map.get(conv) {
            Some(e) if e.account_id == account_id && e.fresh() => {
                let fps = e.fps.lock().unwrap_or_else(|p| p.into_inner());
                let prefix_ok = fps.len() <= history_fps.len()
                    && fps.iter().zip(history_fps).all(|(a, b)| a == b);
                if prefix_ok {
                    let sid = e.session_id();
                    drop(fps);
                    match sid {
                        Some(sid) => CliLookup::Resume(sid),
                        None => CliLookup::Fresh,
                    }
                } else {
                    let first_diff = fps
                        .iter()
                        .zip(history_fps)
                        .position(|(a, b)| a != b)
                        .map(|p| p as i64)
                        .unwrap_or(-1);
                    tracing::info!(
                        conversation_id = conv,
                        stored = fps.len(),
                        incoming = history_fps.len(),
                        first_diff,
                        "cursor-cli:调用方历史分叉,新会话重铺"
                    );
                    drop(fps);
                    CliLookup::Fresh
                }
            }
            _ => CliLookup::Fresh,
        }
    }
}

// ── SSE 辅助 ────────────────────────────────────────────────────────────────

/// 逐段累加文本的 token 估算器。
///
/// 为什么攒字符数而不是每段调一次 [`crate::chat::est_text_tokens`] 再相加:那个函数对
/// ASCII 是 `ceil(n/4)`,**每段都会向上取整** —— 流式几百个 delta 累加下来,光取整误差
/// 就能把估算抬高一倍多(gw-kiro 的 `estimate_output_tokens` 逐帧累加还额外带 `.max(1)`
/// 的地板,问题更重)。攒总数最后算一次,结果与"把整段文本一次性估算"完全一致。
#[derive(Default, Clone, Copy)]
struct TokenTally {
    ascii: u64,
    non_ascii: u64,
}

impl TokenTally {
    fn push(&mut self, text: &str) {
        for c in text.chars() {
            if c.is_ascii() {
                self.ascii += 1;
            } else {
                self.non_ascii += 1;
            }
        }
    }

    /// 与 [`crate::chat::est_text_tokens`] 同口径:ASCII 4 字符/token、非 ASCII 1 字符/token。
    fn tokens(&self) -> u64 {
        self.ascii.div_ceil(4) + self.non_ascii
    }

    /// 乘上标定系数(见 [`TOOL_ROUND_TOKEN_FACTOR`])。
    fn calibrated_tokens(&self) -> u64 {
        (self.tokens() as f64 * TOOL_ROUND_TOKEN_FACTOR) as u64
    }
}

struct SsePhase {
    msg_id: String,
    started: bool,
    next_idx: u32,
    open: Option<(u32, &'static str)>,
    model: String,
    /// 本阶段流出的正文 + thinking + tool_use 参数(自估 output 用)。
    out_tally: TokenTally,
    /// 本阶段**喂进 CLI** 的文本(首轮 = system+prompt+tools,续轮 = 带回的 tool_result)。
    ///
    /// 只算"这一轮新增的输入"。上游那一侧每轮都会重读整个会话,但那部分绝大多数是
    /// 缓存命中(实测一次 6 字符的 prompt 也报 inputTokens=6779 / cacheRead=6016),
    /// 把它算进来会把 cache_read 的口径搅乱 —— 见记忆 caio-cache-billing-and-hot-settings。
    in_tally: TokenTally,
    /// 本请求的**模拟** cache_read(由 [`crate::cache_sim`] 在入口 peek 出来)。
    ///
    /// 为什么自估轮需要它:工具回路的每一轮都是调用方独立的一次 HTTP 请求,发
    /// `tool_use` 那一刻上游还没给 `result`,用量只能自估 —— 而自估这条路
    /// **从来没接过模拟器**(peek/commit 只存在于 `chat.rs` 线协议侧),于是
    /// `cache_read` 结构性恒 0:客户在同一条会话里看到「有缓存的轮」与
    /// 「一点缓存都没有的轮」交替出现,而后者其实上游那侧几乎全是命中。
    ///
    /// ⚠️ 只作用于**自估**路径。上游给了 `result` 真值时一律用真值
    /// (见收尾处的 `usage`),模拟值不参与 —— 模拟是计费策略,真值是事实。
    sim_cache_read: u64,
    /// 本轮上下文总量(见 [`SimSlot::sim_total`]);0 = 未接模拟器,退回按新增量计。
    sim_total: u64,
    /// **本阶段开始前,上游 CLI 会话里已经装着的量**(不含本阶段 `in_tally`)。
    ///
    /// 这是自估 `input` 的基准的另一半:上游每轮读的是整个会话文件,
    /// `ctx_base + in_tally` 才是它这一轮真正吃进去的总量。
    ///
    /// 取值(见 [`start_conv`] 与泵里的阶段切换):
    /// - 首阶段、`--resume` 老会话 → `sim_total`(调用方给的全量历史,上游那边也有);
    /// - 首阶段、**新开**会话 → `0`(上游手里只有我方这一轮喂的,历史根本没送上去);
    /// - 续阶段 → 上一阶段的 `input_basis() + out_tally`(会话又长了这么多)。
    ///
    /// ⚠️ 续阶段**不能**直接改用该轮自己的 `sim_total`。重铺(新开会话)之后,
    /// 调用方仍然每轮带全量历史,`sim_total` 照旧是整段 —— 但上游那边被丢弃的
    /// 前缀永远不会回来。累加式只跟"我方实际喂过什么"走,重铺后自动归零重算。
    ctx_base: u64,
}

impl SsePhase {
    fn with_input(model: &str, in_tally: TokenTally, sim: Option<&SimSlot>, ctx_base: u64) -> Self {
        Self {
            msg_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            started: false,
            next_idx: 0,
            open: None,
            model: model.to_string(),
            out_tally: TokenTally::default(),
            in_tally,
            sim_cache_read: sim.map(|s| s.cache_read).unwrap_or(0),
            sim_total: sim.map(|s| s.sim_total).unwrap_or(0),
            ctx_base,
        }
    }

    /// 本阶段自估 `input` 的基准 = 上游会话已有量 + 本轮喂进去的新增量。
    fn input_basis(&self) -> u64 {
        self.ctx_base.saturating_add(self.in_tally.tokens())
    }

    /// 本阶段的自估用量(上游没给真值时用)。
    ///
    /// ⚠️ 系数**只乘在 output 上**:它补的是隐藏推理(见 [`TOOL_ROUND_TOKEN_FACTOR`]),
    /// input 侧没有对应的隐藏量 —— 我方喂进去多少就是多少,乘系数等于凭空多收。
    ///
    /// ## `input_tokens` 的基准是**上下文总量**,不是"本轮新增 + 命中"
    ///
    /// Anthropic 语义:线缆侧发 `input − cache_read`,故 `input` 必须是总输入。
    /// 曾经这里写 `fresh_in + sim_cache_read`,于是 `uncached ≡ fresh_in` ——
    /// **cache 怎么夹都改不了客户看到的输入**,2026-08-17 实测客户侧输入只剩
    /// 18 个 token(占比 0.02%)。基准错了,夹限就是白做的。
    ///
    /// 现在用 `sim_total`(模拟器算出的 system + tools + 全部历史),与 kiro 的
    /// `final_input_tokens` 同口径 —— 两条通道的客户账单必须可比。
    /// `sim_total` 缺失(未接模拟器)时退回 `fresh_in`,保持旧行为不报缓存。
    ///
    /// ⚠️ 系数**只乘在 output 上**:它补的是隐藏推理(见 [`TOOL_ROUND_TOKEN_FACTOR`]),
    /// input 侧没有对应的隐藏量 —— 我方喂进去多少就是多少,乘系数等于凭空多收。
    ///
    /// `real_cache_read_tokens` 在此**不动**(恒 0):模拟值是计费策略,不是事实断言,
    /// 对账列只认上游自报 —— 与 `chat.rs` 的补偿闸同一条纪律。
    fn estimated_usage(&self) -> ChatUsage {
        // 基准 = `ctx_base + in_tally` —— 上游会话**已有的量**加上本轮喂进去的新增量。
        //
        // ## 为什么不是"本轮新增量"(`in_tally`)
        //
        // 上游每轮读的是整个会话文件,不是我方这一轮送的那几十个字节。基准取新增量
        // 会低估几个数量级 —— 2026-08-18 生产实测(grok-4.6,同一条会话):
        //   id 1585721  input 121,483(**上游真值**)
        //   id 1585725  input      10  ← 紧接着的工具轮,真实上下文只会更大
        //   ……连续 7 条 input 恒为 10(= 喂回的 tool_result JSON 约 40 字符)
        // 客户按 10 个 token 付输入,我方吃掉 12 万 token 的输入成本。
        //
        // ## 为什么也不是 `sim_total`
        //
        // 2026-08-17 标定(10 条真值样本,`sim_total / upstream_input`):
        //   min 0.042 / P25 0.609 / 中位 0.741 / P75 0.847 / max **26.633**
        // 两个方向的误差各有其因,`ctx_base` 的取值规则正是照这两条定的:
        // - 系统性低估约 25%:`sim_total` 里没有 Cursor 注入的服务端 system
        //   (实测约 26k token,见 CHANGELOG 的 cursor 实测段)。少收,方向安全。
        // - 那条 26.6 倍高估出现在**重铺**轮:调用方带回全量历史(模拟器按整段算
        //   76,890),而我方新开了 CLI 会话、实际只喂 2,887 —— 上游手里根本没有那段
        //   历史。拿 `sim_total` 当基准会超收 27 倍。
        //
        // 累加式(见 [`SsePhase::ctx_base`])只跟"我方实际喂过什么"走:老会话
        // `--resume` 时从 `sim_total` 起算(上游那边确实有那些历史),新开会话从 0
        // 起算,重铺后自动归零重算。两个方向的病因都被这条规则挡住。
        let basis = self.input_basis();
        // 缓存夹到 **基准 × cap_ratio**,不是基准本身。
        //
        // 夹到基准本身会让 `cache == input` → 线缆侧 `input − cache = 0` → 客户侧
        // 输入显示 0 且整轮按缓存价 0.1× 计。2026-08-17 实测这样会让 **90% 的请求**
        // 变成全额折扣(63 条里 57 条)。用 cap 收口,与
        // `cache_sim::reported_cache_read` 同源:保证客户侧恒留 `1 − cap` 的余量。
        //
        // ⚠️ 必须 `floor` 不能 `round`:`round` 在小基准上会把上限**抬回基准本身**
        // (基准 10、cap 0.95 → 9.5 → round 得 10 → 占比 1.0000,余量归零)。
        // 2026-08-18 生产实测那 7 条 `input=10 / cache=10` 就是这么来的。
        let cap = crate::cache_sim::billing().cap_ratio;
        let cap = if cap.is_finite() {
            cap.clamp(0.0, 1.0)
        } else {
            crate::cache_sim::DEFAULT_CACHE_CAP_RATIO
        };
        let cache = self.sim_cache_read.min((basis as f64 * cap).floor() as u64);
        ChatUsage {
            input_tokens: basis,
            output_tokens: self.out_tally.calibrated_tokens(),
            cache_read_tokens: cache,
            ..Default::default()
        }
    }

    fn ensure_started(&mut self, out: &OutQueue) {
        if !self.started {
            out.push(OutItem::Item(Ok(StreamItem::Sse(
                crate::chat::message_start_pub(&self.msg_id, &self.model),
            ))));
            self.started = true;
        }
    }

    fn close_block(&mut self, out: &OutQueue) {
        if let Some((idx, _)) = self.open.take() {
            out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
                "content_block_stop",
                json!({"type":"content_block_stop","index":idx}),
            )))));
        }
    }

    fn push_text(&mut self, out: &OutQueue, kind: &'static str, text: &str, is_thinking: bool) {
        // thinking 也计入 output —— 上游就是这么收费的(与线协议 estimate_usage_fallback 同口径)。
        self.out_tally.push(text);
        self.ensure_started(out);
        if let Some((_, k)) = self.open {
            if k != kind {
                self.close_block(out);
            }
        }
        if self.open.is_none() {
            let block = if is_thinking {
                json!({"type":"thinking","thinking":""})
            } else {
                json!({"type":"text","text":""})
            };
            out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
                "content_block_start",
                json!({"type":"content_block_start","index":self.next_idx,"content_block":block}),
            )))));
            self.open = Some((self.next_idx, kind));
            self.next_idx += 1;
        }
        if let Some((idx, _)) = self.open {
            let delta = if is_thinking {
                json!({"type":"thinking_delta","thinking":text})
            } else {
                json!({"type":"text_delta","text":text})
            };
            out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
                "content_block_delta",
                json!({"type":"content_block_delta","index":idx,"delta":delta}),
            )))));
        }
    }

    /// 发 tool_use 块,收尾本阶段(message_delta stop_reason=tool_use + message_stop + End)。
    ///
    /// 用量走**自估**:这一轮上游不会给数(见 [`TOOL_ROUND_TOKEN_FACTOR`])。早先这里
    /// 硬写 `{"input_tokens":0,"output_tokens":0}` 且不 push [`StreamItem::Usage`],于是
    /// 以工具调用收尾的请求全部记 0 —— 2026-08-17 生产实测占 cursor 全部请求的 **56%**
    /// (3 小时 1259 条里 632 条,相关性 100%),客户在 new-api 面板看到的就是
    /// 「输入 11 万 / 输出 0」。正文其实照发,只是账没记(线协议侧早就有
    /// `estimate_usage_fallback` 兜同一个坑,CLI 驱动是后加的,漏了)。
    fn finish_tool_use(&mut self, out: &OutQueue, tool_use_id: &str, name: &str, args: &Value) {
        // 纯工具调用轮的产出大头是**参数 JSON**(正文可以是零个字),漏掉它 output≈0。
        // 与线协议的 tool_call_tokens 同口径:名字 + 整个参数对象序列化后计字。
        self.out_tally.push(name);
        self.out_tally.push(&args.to_string());
        self.ensure_started(out);
        self.close_block(out);
        let idx = self.next_idx;
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,
                   "content_block":{"type":"tool_use","id":tool_use_id,"name":name,"input":{}}}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "content_block_delta",
            json!({"type":"content_block_delta","index":idx,
                   "delta":{"type":"input_json_delta","partial_json":args.to_string()}}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "content_block_stop",
            json!({"type":"content_block_stop","index":idx}),
        )))));
        let usage = self.estimated_usage();
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_delta",
            json!({"type":"message_delta",
                   "delta":{"stop_reason":"tool_use","stop_sequence":null},
                   "usage":crate::chat::delta_usage_json_pub(&usage)}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_stop",
            json!({"type":"message_stop"}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Usage(usage))));
        out.push(OutItem::End);
    }

    /// 正常收尾(end_turn + 真实用量)。
    fn finish_done(&mut self, out: &OutQueue, usage: &ChatUsage) {
        self.ensure_started(out);
        self.close_block(out);
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_delta",
            json!({"type":"message_delta",
                   "delta":{"stop_reason":"end_turn","stop_sequence":null},
                   "usage":crate::chat::delta_usage_json_pub(usage)}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_stop",
            json!({"type":"message_stop"}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Usage(usage.clone()))));
        out.push(OutItem::End);
    }

    fn finish_error(&mut self, out: &OutQueue, err: UpstreamError) {
        if self.started {
            self.close_block(out);
        }
        out.push(OutItem::Item(Err(err)));
        out.push(OutItem::End);
    }
}

/// 把 CLI `result` 事件的 usage 收成 [`ChatUsage`]。
///
/// ## `inputTokens` 是**总输入**(含缓存),不要再加 cached
///
/// 实测钉死(生产抓的原始 NDJSON,提示词是「只回一个词:ok」约 10 字符):
/// ```text
/// {"inputTokens":6779,"outputTokens":35,"cacheReadTokens":6016,"cacheWriteTokens":0}
/// ```
/// 10 字符的提示词不可能有 6779 token 的**新增**输入 —— 6779 是总量
/// (system/AGENTS.md 等),其中 6016 命中缓存,未命中 ≈763。所以
/// `inputTokens ⊇ cacheReadTokens`,与 [`ChatUsage::input_tokens`] 的"总输入"
/// 契约天然一致,**原样填**即可。
///
/// ⚠️ 我曾据「生产上 39% 的记录 `cache_read > input`」推断上游是"未命中新增"语义,
/// 于是改成 `up_in + cached` —— **那是错的**,会给绝大多数正常轮次凭空加一倍输入。
/// 上面那条单次调用的实测是判据:`cache > input` 不能反推字段语义。
///
/// ## `cache > input` 的真实成因与处理
///
/// 生产实测(近 4h grok-4.6 有缓存记录 127 条)有 **49 条(39%)`cache > input`**,
/// 最极端 239 倍(`input=54 / cache=12928`)。
///
/// 2026-08-17 开 `CURSOR_CLI_DUMP_NDJSON` 抓真实会话**钉死了成因**:`cache > input`
/// 只出现在**走 MCP 桥的多轮会话**上,单轮会话一律 `input > cache`:
///
/// ```text
/// 桥会话(6 次桥调用) input=9844  cacheRead=35840  ← cache 是 input 的 3.6 倍
/// 桥会话(6 次桥调用) input=4868  cacheRead=38272
/// 单轮会话(0 次)     input=50227 cacheRead=5888   ← 正常
/// 单轮会话(0 次)     input=50165 cacheRead=6016
/// ```
///
/// 即 `cacheReadTokens` 跨 CLI **内部多次模型调用**累加,而 `inputTokens` 只算末次。
/// 两个字段不同口径,**不可相减、也不可据此反推语义**。
///
/// ## `result` 只结算末轮,不跨调用方 HTTP 轮(与自估**不重叠**)
///
/// 同批取证顺带排掉了「最终真值与前面各轮自估重复计费」这个担忧(对抗评审的阻断级
/// 问题之一):四条走桥的多轮会话,其 `result.usage` 都精确等于**单独一条**
/// `request_log`,而非多轮之和 —— 桥挂起期间每轮走 `estimated_usage` 自估,
/// CLI 进程退出时那一次 `result` 只落在最后一轮。所以两条路径各管一段,不重复。
/// ⚠️ 若将来改动收尾时机(比如让 result 补算整条会话),这条不变式就没了,要重新验。
///
/// ## 处理:`cache > input` 时**丢弃**这个不可信的 cache,不封顶到 input
///
/// 不去"修正"input(重构不出可信的总量)。对 cache 有两种处理,**第一版选错了**:
///
/// - ❌ **封顶到 input**:恢复了 `input ≥ cache_read` 不变式,但让 `cache == input`,
///   于是线缆侧 `input − cache = 0` —— **客户面板的「输入」列还是 0**,只是从"偶发"
///   变成了"必然"。2026-08-17 部署后实测:5 条里 3 条(60%)被封成全等,客户看到的
///   毛病一点没好。而且 `cache == input` 在语义上是**假断言**("本轮输入 100% 命中"),
///   真实情况只是两个字段口径不同。
/// - ✅ **丢弃 cache(置 0)**:`input` 是可信的(单次调用实测证明它就是总量),
///   cache 是跨内部调用累加的、与这个 input 不同口径 —— 两个数没有可信的相对关系,
///   那就只保留可信的那个。客户侧 `input − 0 = input`,输入列显示真实总量;
///   代价是这些轮次**不给缓存折扣**(客户按全价付),方向偏向"宁可少给折扣也不谎报"。
///
/// ⚠️ 别再改回封顶:那会让「输入 0」以更高的频率回来。
///
/// 丢弃时 warn:这是上游口径异常的信号,也是这些轮次没拿到折扣的原因,要可查。
fn usage_from_result(u: &Value) -> ChatUsage {
    let input = u.get("inputTokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let reported_cache = u.get("cacheReadTokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let cached = if reported_cache > input {
        tracing::warn!(
            input,
            reported_cache,
            "cursor-cli:上游 result 的 cacheReadTokens 超过 inputTokens(跨内部调用累加,与本轮 input 不同口径),已丢弃该缓存值(本轮不给折扣,但输入列保持真实)"
        );
        0
    } else {
        reported_cache
    };
    ChatUsage {
        input_tokens: input,
        output_tokens: u.get("outputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
        cache_read_tokens: cached,
        // 上游自报即**真实**命中,同步对账列(与线协议 `usage_from_upstream` 同一条
        // 纪律:漏填会让面板「真实缓存」列永远是 0)。存封顶后的值:超过总输入的
        // 数字放进对账列只会污染对账。
        real_cache_read_tokens: cached,
        ..Default::default()
    }
}

/// 真值轮的**缓存回落**:上游 cache 不可用(被判不可信而丢弃 → 0)时,用模拟值顶上。
///
/// 为什么必须回落而不是留 0:`usage_from_result` 丢弃的是"上游那个数不能用",
/// 不代表本轮真的零命中。不回落的后果是**末轮(有真值那轮)反而一点缓存都没有**,
/// 而中间自估轮有缓存 —— 客户看到"突然某一条完全没命中"(2026-08-17 用户报障)。
/// 实测一条:input=85951、上游 cache=162176(累加值)被丢弃 → 计 0,而模拟器同一
/// 请求给出 57336 命中,客户因此按全价付了约 2.4 倍。
///
/// 线协议侧早有同一条闸(`chat.rs` 的 `cache_read_tokens == 0 && sim_cache_read > 0`),
/// CLI 驱动后加、漏了,这里补齐。
///
/// 只动**计费列**;`real_cache_read_tokens` 由调用方保持(对账列只认上游自报)。
/// 夹到 `input` 之内以保住 `input ≥ cache_read`(否则线缆侧 saturating_sub 归 0)。
fn fallback_cache_from_sim(usage: &mut ChatUsage, sim_cache_read: u64) -> Option<u64> {
    if usage.cache_read_tokens > 0 || sim_cache_read == 0 {
        return None;
    }
    // ⚠️ 上限用 **input × cap_ratio**,不是 input 本身。
    //
    // 夹到 input 会让 `cache == input` → 客户侧输入 0 → **整轮按缓存价(0.1×)计**。
    // 2026-08-17 部署后实测:63 条里 **57 条(90%)** 被夹成全额折扣,总输入 716,826
    // 中 635,150(88.6%)按 0.1× 计 —— 远超我部署前估的 19%,方向是我方大幅贴钱。
    // 根因是自估轮的 `input` 只量"本轮实际喂进 CLI 的量"(续轮就是一条 tool_result,
    // 几十到几千 token),而模拟命中按**完整上下文**算(几万),一夹必然相等。
    //
    // 用 cap 收口:与 `cache_sim::reported_cache_read` 的 `cap_ratio` 同源同语义
    // (「杜绝假到全命中」),保证客户侧输入恒留 `1 − cap` 的余量,不再出现整轮 0.1×。
    let cap = crate::cache_sim::billing().cap_ratio;
    let cap = if cap.is_finite() { cap.clamp(0.0, 1.0) } else { crate::cache_sim::DEFAULT_CACHE_CAP_RATIO };
    // `floor` 不能 `round`:`round` 在小 input 上会把上限抬回 input 本身
    // (input 10、cap 0.95 → 9.5 → 10),余量归零,等于没夹。
    let ceiling = (usage.input_tokens as f64 * cap).floor() as u64;
    let fallback = sim_cache_read.min(ceiling);
    if fallback == 0 {
        return None;
    }
    usage.cache_read_tokens = fallback;
    Some(fallback)
}

/// drain 一条响应流(到 End 为止)。
fn drain_stream(out: Arc<OutQueue>) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    // 状态是 `Option`:超时那一下要**先发一条错误、再终止**,不能继续 pop ——
    // 否则下一次 poll 又等 90s,变成每 90s 吐一条错误的死循环。
    futures::stream::unfold(Some(out), |st| async move {
        let out = st?;
        match tokio::time::timeout(DRAIN_IDLE_TIMEOUT, out.pop()).await {
            Ok(OutItem::Item(it)) => Some((it, Some(out))),
            Ok(OutItem::End) => None,
            Err(_) => {
                // 泵还活着(它可能正卡在别处),所以只结束**本次响应**,不动 CLI 进程:
                // 调用方重试会走 cli_lookup,该重铺就重铺。
                tracing::warn!(
                    secs = DRAIN_IDLE_TIMEOUT.as_secs(),
                    "cursor-cli:调用方一侧连续无事件,按本轮失败收尾"
                );
                let e = UpstreamError::new(
                    UpstreamErrorKind::Other,
                    format!("cursor-cli: 连续 {}s 没有任何输出", DRAIN_IDLE_TIMEOUT.as_secs()),
                );
                Some((Err(e), None))
            }
        }
    })
}

// ── 泵任务 ──────────────────────────────────────────────────────────────────

struct PumpArgs {
    conv: Arc<CliConv>,
    cli: tokio::process::Child,
    /// 桥 socket(CLI 拉起的桥进程回连;有工具时才有)。
    sock: Option<tokio::net::UnixStream>,
    echo_model: String,
    /// token 轮换观测:auth.json 路径 + 开泵时的已知 token + 捕获上报表。
    /// CLI 中途轮换后若崩溃,没捕获到 = 号砖(旧 refresh_token 已作废)。
    auth_file: PathBuf,
    known_token: String,
    updates: TokenUpdates,
    /// 首轮喂给 CLI 的输入量(system + prompt + 工具定义)。工具轮次自估用量要用
    /// (见 [`SsePhase::in_tally`]);续轮的输入是带回的 tool_result,在泵里现算。
    first_in_tally: TokenTally,
    /// 首阶段的 `ctx_base`(见 [`SsePhase::ctx_base`]):`--resume` 老会话时 =
    /// `sim_total`(上游那边有那段历史),新开会话时 = 0(上游只有我方这轮喂的)。
    first_ctx_base: u64,
    /// 首轮(= 开启这条 CLI 会话的那次请求)的模拟缓存槽。泵**独占**它,
    /// 后续阶段的槽由 `resume_conv` 经 responder 通道送进来 —— 见 [`PendingSlot`]。
    sim: Option<SimSlot>,
}

/// 泵:读 CLI stdout + 桥 socket,把事件翻译成 SSE 写进 OutQueue。
/// 桥调用处挂起(等下一轮网关请求喂结果),CLI 进程全程存活。
async fn pump(mut a: PumpArgs) {
    let out = a.conv.out.clone();
    // 本阶段的模拟槽:泵独占。阶段切换时换成 resume 送来的那个(见 PendingSlot)。
    let mut sim: Option<SimSlot> = a.sim.take();
    let mut phase =
        SsePhase::with_input(&a.echo_model, a.first_in_tally, sim.as_ref(), a.first_ctx_base);
    let mut state = NdjsonState::default();
    let started = Instant::now();
    let mut last_auth_poll = Instant::now();
    let mut known_token = a.known_token.clone();

    let stdout = a.cli.stdout.take().expect("stdout piped");
    let stderr = a.cli.stderr.take();
    let stderr_task = stderr.map(|s| {
        tokio::spawn(async move {
            let mut acc = String::new();
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                acc.push_str(&l);
                acc.push('\n');
                if acc.len() > 4096 {
                    acc.drain(..2048);
                }
            }
            acc
        })
    });

    let mut cli_lines = BufReader::new(stdout).lines();
    let (mut sock_lines, mut sock_w) = match a.sock {
        Some(s) => {
            let (r, w) = s.into_split();
            (Some(BufReader::new(r).lines()), Some(w))
        }
        None => (None, None),
    };

    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 结果:Ok(()) 正常完结;Err 进 finish_error。
    let outcome: Result<(), UpstreamError> = 'outer: loop {
        tokio::select! {
            line = cli_lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => break 'outer Ok(()), // EOF:CLI 退出
                    Err(e) => break 'outer Err(UpstreamError::network(format!("读 cursor-cli 输出失败: {e}"))),
                };
                // 逆向排障:原始 NDJSON 落盘(仅设了 CURSOR_CLI_DUMP_NDJSON 时)。
                if let Ok(f) = std::env::var("CURSOR_CLI_DUMP_NDJSON") {
                    use std::io::Write as _;
                    if let Ok(mut fp) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
                        let _ = writeln!(fp, "{}", line);
                    }
                }
                match handle_line(&mut state, &line) {
                    Ev::Nothing => {}
                    // 思考一律透传(不看客户端要不要):agent 客户端需要思考块,
                    // 丢掉等于丢进展信号,还会让收侧 stall 看门狗误判掐流。
                    Ev::Thinking(t) => phase.push_text(&out, "thinking", &t, true),
                    Ev::Delta(t) => phase.push_text(&out, "text", &t, false),
                    Ev::Done => break 'outer Ok(()),
                }
                if let Some(sid) = state.session_id.clone() {
                    let mut slot = a.conv.cli_session_id.lock().unwrap_or_else(|p| p.into_inner());
                    if slot.is_none() {
                        *slot = Some(sid);
                    }
                }
            }
            call = async {
                match &mut sock_lines {
                    Some(l) => l.next_line().await,
                    None => std::future::pending().await,
                }
            } => {
                let call = match call {
                    Ok(Some(c)) => c,
                    _ => break 'outer Err(UpstreamError::new(UpstreamErrorKind::Other, "cursor-cli 桥连接中断".to_string())),
                };
                let Ok(v) = serde_json::from_str::<Value>(&call) else { continue };
                let Some(call) = v.get("call") else { continue };
                let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let args = call.get("args").cloned().unwrap_or(json!({}));
                let tool_use_id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                tracing::info!(tool = %name, id = %tool_use_id, "cursor-cli:模型调用桥接工具,转成 tool_use 并挂起");

                // ⚠️ **先把本阶段的槽取到手,再挂 pending**。`pending` 一旦可见,调用方
                // 就能带着 tool_result 回来(`finish_tool_use` 只是入队、不等于送达),
                // 而那条路会经 responder 送来**下一轮**的槽。先取后挂,本阶段要提交的
                // 东西就已经在局部变量里,不可能被后来者换掉。
                let this_sim = sim.take();

                let (tx_slot, rx_slot) =
                    oneshot::channel::<(Result<String, String>, Option<SimSlot>)>();
                {
                    let mut p = a.conv.pending.lock().unwrap_or_else(|p| p.into_inner());
                    *p = Some(PendingSlot { tool_use_id: tool_use_id.clone(), responder: tx_slot });
                }
                phase.finish_tool_use(&out, &tool_use_id, &name, &args);
                // 提交本阶段指纹。**判据是"这个 Anthropic 响应已经交付给调用方"**,
                // 不是"整条 CLI 会话成功" —— 对调用方而言这就是一次完整的成功响应
                // (有正文、有 tool_use、stop_reason=tool_use),它已经据此计费了。
                // 下一轮带 tool_result 回来时那段历史确实在上游手里,该命中。
                //
                // 之后调用方断线 / CLI 报错**不该回滚**它:那是**下一**轮的失败,
                // 而且真发生时 `ConvRegistry` 的分叉校验会换掉 conversation_id
                // (= 换掉模拟键),旧条目自然命不中。两处收尾的 commit 判据由此统一
                // (对抗评审 high#1/medium#4:提交时机不能取决于模型是否恰好走了
                // tool_use 分支)。
                if let Some(s) = this_sim {
                    s.commit();
                }

                // 挂起等调用方结果(带 TTL)。连同**下一阶段的模拟槽**一起收下。
                let res = tokio::time::timeout(PENDING_TTL, rx_slot).await;
                let (reply, next_sim) = match res {
                    Ok(Ok((Ok(text), s))) => (json!({"result": text}), s),
                    Ok(Ok((Err(err), s))) => (json!({"error": err}), s),
                    Ok(Err(_)) => (json!({"error": "网关注销了这次调用"}), None),
                    Err(_) => (
                        json!({"error": format!("等待调用方 tool_result 超时({}s)", PENDING_TTL.as_secs())}),
                        None,
                    ),
                };
                // 新阶段(下一个 Anthropic 响应)重新开始计数。**在拿到 reply 之后**才重建
                // —— 喂回 CLI 的这段文本就是下一阶段的输入,要计进它的 in_tally。
                // (重建时机必须早于下一次 phase.push_text,这里满足。)
                let mut next_in = TokenTally::default();
                next_in.push(&reply.to_string());
                // 上游会话到这一刻装着的量 = 上一阶段的输入基准 + 上一阶段流出的正文。
                // 下一阶段的 input 基准从这里接着往上加(见 SsePhase::ctx_base)。
                // 必须**在换 phase 之前**算,换完就读不到上一阶段的 tally 了。
                let next_ctx = phase
                    .input_basis()
                    .saturating_add(phase.out_tally.calibrated_tokens());
                // 槽随通道换成**这一轮自己的**:新阶段对应调用方的下一次独立请求,
                // 它按自己的历史 peek(历史更长 → 命中更多)。所有权跟着结果走,
                // 不经会话共享字段 —— 并发请求不可能互相覆盖。
                sim = next_sim;
                phase = SsePhase::with_input(&a.echo_model, next_in, sim.as_ref(), next_ctx);
                if let Some(w) = &mut sock_w {
                    let _ = w.write_all(reply.to_string().as_bytes()).await;
                    let _ = w.write_all(b"\n").await;
                    let _ = w.flush().await;
                }
            }
            _ = tick.tick() => {
                if started.elapsed() > CLI_TIMEOUT {
                    break 'outer Err(UpstreamError::new(
                        UpstreamErrorKind::Other,
                        format!("cursor-cli 单轮超过 {}s,杀进程", CLI_TIMEOUT.as_secs()),
                    ));
                }
                // CLI 自刷新会回写 auth.json;中途轮换若没被捕获,CLI 一崩新 token
                // 就丢了,而旧 refresh_token 已被上游作废 → 号砖。每 5s 看一眼,
                // 变了就上报(worker 周期任务 CAS 落库)。
                if last_auth_poll.elapsed() >= Duration::from_secs(5) {
                    last_auth_poll = Instant::now();
                    if let Some((at, rt)) = read_auth_creds(&a.auth_file) {
                        if at != known_token {
                            let exp = crate::auth::token_expires_at(&at);
                            report_token_update(&a.updates, &a.conv.account_id, &at, rt.as_deref(), exp);
                            known_token = at;
                        }
                    }
                }
            }
        }
    };

    // ── 收尾 ────────────────────────────────────────────────────────────────
    //
    // 顺序与超时都是**必需的**,不是防御性冗余。这里曾经是
    // `kill_procs()`(空壳)+ 无限 `stderr_task.await`,构成一个循环等待:
    // 子进程不死 → stderr 不 EOF → await 不返回 → Child 不 drop → kill_on_drop 不触发
    // → 子进程不死。详见 [`CliConv::kill_procs`] 的注释与现场取证。
    //
    // 现在:① 先 kill 整组(主进程 + node 帮工);② tokio 自己的 kill 兜一手,顺带
    // reap 掉僵尸;③ 收 stderr 时**带超时**,超时就 abort —— 即便 kill 因为任何原因
    // 没生效(权限、pid 复用、组已变),泵也一定能返回,`Child` 一定会 drop。
    a.conv.kill_procs();
    // tokio 的 kill 只打主进程,但它会 wait 回收,避免留僵尸;组已被杀时报错属正常。
    let _ = a.cli.kill().await;
    if let Some(mut t) = stderr_task {
        const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
        // 传 `&mut t` 而不是 `t`:超时后还要 `abort()`。光丢 JoinHandle 在 tokio 里是
        // **detach 而非取消**,那个 task 会继续攥着 stderr 读端不放。
        match tokio::time::timeout(STDERR_DRAIN_TIMEOUT, &mut t).await {
            Ok(Ok(s)) => {
                if !s.trim().is_empty() {
                    tracing::debug!(stderr = %s.chars().take(300).collect::<String>(), "cursor-cli stderr");
                }
            }
            Ok(Err(_)) => {} // task 自身 panic/被取消:stderr 拿不到,不影响收尾
            Err(_) => {
                t.abort();
                // 走到这里说明 kill 没能让 stderr EOF —— 是真出问题了,warn 出来。
                tracing::warn!(
                    account = %a.conv.account_id,
                    secs = STDERR_DRAIN_TIMEOUT.as_secs(),
                    "cursor-cli:杀完进程组后 stderr 仍未 EOF,放弃收集(泵照常返回)"
                );
            }
        }
    }

    match outcome {
        Ok(()) => {
            // `upstream_raw_cache` = 上游 result 里**原始**的 cacheReadTokens,
            // 未经 `usage_from_result` 的丢弃/口径处理。标定必须用它,不能用
            // `usage.real_cache_read_tokens`:那一列在「cache > input」时已被置 0,
            // 于是标定会把「上游报了但我方丢弃」误记成「上游真值 0」,算出假的
            // ratio=0.000(2026-08-17 首批样本就这么被污染了一条)。
            let (ok, mut usage, upstream_raw_cache) = match &state.result {
                Some(r) if r.get("subtype").and_then(|s| s.as_str()) == Some("success") => {
                    let u = r.get("usage").cloned().unwrap_or_default();
                    let raw = u.get("cacheReadTokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    (true, usage_from_result(&u), raw)
                }
                other => {
                    tracing::warn!(result = ?other.as_ref().map(|r| r.to_string().chars().take(200).collect::<String>()),
                        "cursor-cli: 未见成功 result 事件");
                    (false, ChatUsage::default(), 0)
                }
            };
            if ok {
                // ── 真值不可用时回落模拟值(**不要归零**)────────────────────────
                //
                // `usage_from_result` 在「上游 cache > input」(跨内部调用累加,生产常态)
                // 时会丢弃那个不可信的 cache → `cache_read_tokens = 0`。但那只说明
                // **上游那个数不能用**,不代表这一轮真的没有缓存命中 —— 同一请求模拟器
                // 往往estimate 出可观的命中量。
                //
                // 不回落的后果(2026-08-17 用户报障 + 标定日志双向确认):**末轮(有真值那轮)
                // 反而一点缓存都没有**,而中间自估轮有缓存,客户看到的就是"突然某一条
                // 完全没有命中"。实测一条:input=85951、上游 cache=162176 被丢弃 → 计 0,
                // 而模拟器同一请求给出 57336 命中 —— 客户按全价付了 ¥0.0274,
                // 若用上模拟值约 ¥0.0103,**多付 2.4 倍**。
                //
                // 线协议侧早有同一条闸(`chat.rs` 的 `if usage.cache_read_tokens == 0
                // && sim_cache_read > 0`),CLI 驱动是后加的,漏了。这里补齐。
                //
                // 只动**计费列**:`real_cache_read_tokens` 保持 0(对账列只认上游自报,
                // 模拟值是计费策略、不是事实断言)。夹到 input 之内保住
                // `input ≥ cache_read` 不变式。
                if let Some(fallback) =
                    fallback_cache_from_sim(&mut usage, sim.as_ref().map(|s| s.cache_read).unwrap_or(0))
                {
                    tracing::info!(
                        account = %a.conv.account_id,
                        upstream_raw_cache,
                        upstream_input = usage.input_tokens,
                        sim_fallback = fallback,
                        "cursor-cli:上游缓存值不可用,回落模拟值计费(避免整轮按全价收)"
                    );
                }

                // ── 模拟器标定(只观测,不参与计费)────────────────────────────
                //
                // 这一轮上游给了**真值**,而我们手上同时有模拟器对同一请求的估算 ——
                // 两个数并排打出来,就是校准 `read_multiplier` 的唯一实测依据。
                //
                // 为什么必须在这里打:自估轮(工具回路中间各轮,占 72%)永远拿不到真值,
                // 真值轮(末轮,占 28%)默认又不碰模拟器,两个数从不在同一条记录里出现,
                // 于是「模拟器估得准不准」在库里**无法离线回答**。
                //
                // 口径说明:`sim_hit` 是模拟器**夹限前**的原始命中量,`real` 是上游自报。
                // 比值 = real / sim_hit —— >1 说明模拟器低估(客户少拿折扣),
                // <1 说明高估(我方白送)。`multiplier` 该取多少看这个比值的中位数,
                // 而不是沿用 kiro 的 1.8(那是 kiro 自己那套 tokenizer 标出来的)。
                if let Some(s) = sim.as_ref() {
                    // 用**上游原始值**做标定(见上面 upstream_raw_cache 的注释);
                    // `billed_cache` 另列出来,便于看"丢弃/夹限改了多少"。
                    tracing::info!(
                        account = %a.conv.account_id,
                        upstream_raw_cache,
                        billed_cache = usage.cache_read_tokens,
                        sim_raw_hit = s.raw_hit,
                        sim_reported = s.cache_read,
                        sim_total = s.sim_total,
                        upstream_input = usage.input_tokens,
                        // upstream_raw / sim_raw:>1 模拟器低估(客户少拿折扣)、
                        // <1 高估(我方白送)。取这个比值的中位数就是 multiplier 的实测取值。
                        // 两边都是"未经运营参数加工"的数,比值才有物理意义。
                        ratio_real_over_raw = if s.raw_hit > 0 {
                            format!("{:.3}", upstream_raw_cache as f64 / s.raw_hit as f64)
                        } else {
                            "sim_raw=0".to_string()
                        },
                        "cursor-cli:缓存标定样本(真值 vs 模拟,仅观测不计费)"
                    );
                }
                // 成功收尾:提交本阶段指纹(判据同 tool_use 处 —— 这个响应交付了)。
                // 失败分支**不提交**:那一轮调用方拿到的是错误,没有可命中的历史。
                if let Some(s) = sim.take() {
                    s.commit();
                }
                phase.finish_done(&out, &usage);
            } else {
                phase.finish_error(&out, UpstreamError::new(
                    UpstreamErrorKind::Other,
                    "cursor-cli 未成功收尾(无 success result)".to_string(),
                ));
            }
        }
        Err(e) => phase.finish_error(&out, e),
    }
}

// ── 对外入口 ────────────────────────────────────────────────────────────────

/// 首阶段的 input 基准:返回 `(ctx_base, in_tally)`。
///
/// **两条分支互斥,绝不能相加** —— 它们是同一个量的两种算法。上游这一轮真正吃进去的,
/// 就是那条 CLI 会话文件此刻装着的全部内容:
///
/// - **`--resume` 老会话**(`resumed_sim_total > 0`):装着调用方给的那整段历史,
///   `sim_total` 就是它的估计(口径 = system + tools + 全部历史,见
///   [`crate::cache_sim::fingerprints_from_context`])。此时 `in_tally` 必须**留空**:
///   prompt 与 tools 已经计在 `sim_total` 里,再叠一次是重复计费(tools 对 Claude Code
///   一类客户能有上万 token)。`sim_total` 偏低约 25%(少的是 Cursor 注入的服务端
///   system),方向安全。
///
/// - **新开会话(含重铺)**:调用方照旧带全量历史,但我方只把最后一段用户输入喂了上去
///   —— 那段历史上游根本没有,拿 `sim_total` 当基准会超收(标定里那条 **26.6 倍**
///   离群正是这种轮次)。基准 = 我方实际喂进去的 `system + tools + prompt`。
///   ⚠️ **system 必须计**:它经 AGENTS.md 落盘([`prepare_home`]),CLI 每轮重读,
///   上游照样按它计费。2026-08-18 受控实测(新会话、空 system、两个小工具):
///   我方自估首轮 48,同一进程退出时上游真值 **16,821** —— 差的 16.5k 是每请求的
///   固定地板(AGENTS.md + Cursor 注入的服务端 system),计上 system 补掉我方能算的那半。
///
/// 模拟器没接上(`sim_total` 为 0)时退回按实际喂入量算,总比 0 好。
fn first_phase_basis(
    resumed_sim_total: u64,
    system: &str,
    prompt: &str,
    tools: &[crate::run::ToolDef],
) -> (u64, TokenTally) {
    let mut tally = TokenTally::default();
    if resumed_sim_total > 0 {
        return (resumed_sim_total, tally);
    }
    tally.push(system);
    tally.push(prompt);
    for t in tools {
        tally.push(&t.name);
        tally.push(&t.description);
        tally.push(&t.schema);
    }
    (0, tally)
}

/// 开一条新的 CLI 会话(新 spawn;`lookup` 决定带不带 --resume),返回首阶段响应流。
#[allow(clippy::too_many_arguments)]
pub async fn start_conv(
    cfg: &CliDriverConfig,
    convs: &CliConversations,
    conv_key: &str,
    account_id: &str,
    // 账号出口代理(`extra.proxy`)。None/空 = 直连。**必须显式传**:env_clear()
    // 之后不继承任何代理变量,漏传即静默直连(见下方设置处的注释)。
    proxy: Option<&str>,
    home: &Path,
    ws: &Path,
    cli_model: &str,
    prompt: &str,
    // 调用方 system 提示(已经由 `prepare_home` 落进工作区 AGENTS.md)。
    // 只用于**新会话**的 input 基准:CLI 每轮都会重读 AGENTS.md,上游照样按它计费,
    // 漏算就是我方贴钱。续会话不用它 —— `SimSlot::sim_total` 的口径本来就含 system。
    system: &str,
    resume_sid: Option<String>,
    tools: &[crate::run::ToolDef],
    echo_model: &str,
    updates: TokenUpdates,
    // 本轮的模拟缓存材料(未 peek)。None = 不模拟,自估轮 cache_read 退回 0。
    sim: Option<SimRequest>,
) -> Result<gw_core::provider::ChatStream, UpstreamError> {
    // 同号 spawn 串行化:mcp.json 是每号一份的静态路径,桥 socket 每请求一条,
    // 等桥连上(或确认无工具)后再放行下一个,避免后一个请求改写 mcp.json 被
    // 前一个 CLI 读到。窗口 ~1s。用 tokio 锁:要跨 await 持有。
    static SPAWN_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _gate = SPAWN_GATE.lock().await;

    let io = |what: &str, e: std::io::Error| {
        UpstreamError::new(UpstreamErrorKind::Other, format!("cursor-cli: {what}失败: {e}"))
    };

    // 工具桥:mcp.json + tools 文件 + unix socket 监听。**桥进程由 CLI 经
    // mcp.json 自己拉起**(stdio 是 MCP 的生命线),网关只监听回连 —— 
    // 第一版自己抢跑了一个桥,把唯一一次 accept 用掉,CLI 拉起的桥反而连不上。
    let mut listener = None;
    if !tools.is_empty() {
        let req_id = uuid::Uuid::new_v4().simple().to_string();
        let bridge_dir = home.join("bridge");
        std::fs::create_dir_all(&bridge_dir).map_err(|e| io("创建 bridge 目录", e))?;
        let sock_path = bridge_dir.join(format!("{req_id}.sock"));
        let tools_path = bridge_dir.join(format!("{req_id}.tools.json"));
        let mcp_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": serde_json::from_str::<Value>(&t.schema).unwrap_or(json!({"type":"object"})),
                })
            })
            .collect();
        std::fs::write(&tools_path, serde_json::to_string(&mcp_tools).unwrap())
            .map_err(|e| io("写 tools 文件", e))?;
        std::fs::write(
            home.join(".cursor/mcp.json"),
            json!({"mcpServers": {"gwtools": {
                "command": cfg.self_exe.to_string_lossy(),
                "args": ["--mode", "cursor-mcp-bridge",
                         "--sock", sock_path.to_string_lossy(),
                         "--tools", tools_path.to_string_lossy()],
            }}}).to_string(),
        )
        .map_err(|e| io("写 mcp.json", e))?;
        let l = tokio::net::UnixListener::bind(&sock_path).map_err(|e| io("绑 bridge socket", e))?;
        // 桥以本账号的 uid 回连:socket 0600 + 属主=该 uid,别号的 CLI(不同 uid)
        // 连不上 —— 共享 nobody 时这里曾是 0666,任何被注入的 CLI 都能往别人的
        // 桥里注结果(对抗审查共识 S0-2)。bridge/ 还在 700 的 HOME 里,路径本身
        // 就不可达,这里是第二道。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
            let uid = account_uid(account_id);
            let _ = std::process::Command::new("chown")
                .arg(format!("{uid}:{uid}"))
                .arg(&sock_path)
                .status();
        }
        listener = Some(l);
    }

    let mut cmd = Command::new(&cfg.bin);
    cmd.arg("-p")
        .arg("--trust")
        .arg("--mode")
        .arg("ask") // 安全闸:read-only。绝不加 --force。
        .arg("--model")
        .arg(cli_model)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--stream-partial-output");
    if !tools.is_empty() {
        cmd.arg("--approve-mcps");
    }
    // 续会话与新开会话在计费基准上是两种东西(见下面的 `first_ctx_base`),
    // 而 `resume_sid` 后面会被移进 `CliConv`,所以在这里先把这一位留下来。
    let resumed = resume_sid.is_some();
    if let Some(sid) = &resume_sid {
        cmd.arg("--resume").arg(sid);
    }
    // prompt 走 stdin 不走 argv:折叠重铺的 prompt 能有几百 KB,
    // argv 上限(~2MB 但单参数 128KB)会直接 E2BIG(实测 "Argument list too long")。
    cmd.current_dir(ws)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // 出口:账号配了代理就必须透传给 CLI。
    //
    // ⚠️ `env_clear()` 之后**什么都不继承**,所以漏了这段的后果不是"退回容器默认代理",
    // 而是**静默直连**。2026-08-17 实测:三个代理号切到 CLI 驱动后全部失败,而记忆里
    // 直连出口的封号率是 59.5%(代理 0%)—— 也就是说漏这段不只是不通,是在烧号。
    //
    // cursor-agent 认不认这几个变量是**实测**过的,不是照惯例猜的:把 `HTTPS_PROXY`
    // 指向 `127.0.0.1:1`,`--list-models` 当场 `ECONNREFUSED 127.0.0.1:1`;不设则正常。
    // 所以它走的是认环境变量的 HTTP 客户端,`NODE_USE_ENV_PROXY` 不需要(加了也不变)。
    if let Some(p) = proxy.map(str::trim).filter(|p| !p.is_empty()) {
        // 大小写两套都给:不同库只认其中一套(curl 系认小写,多数 Node/Python 库认大写)。
        for k in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy"] {
            cmd.env(k, p);
        }
        // 本地回环别走代理,否则 MCP 桥那条 unix/loopback 通路会被绕进代理。
        cmd.env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost");
    }
    // 降权:每账号独立 uid 运行(见 account_uid)。ask 模式的只读 shell 仍会真跑
    // 命令 —— 不能让模型用 `cat` 读走 /app/data 的号库,也不能让它读走**别号**
    // 的 HOME(共用 nobody 时 700/600 不挡同 uid)。仅在 root(容器)下启用;
    // 本机开发自动跳过(非 root setuid 会 EPERM)。
    #[cfg(unix)]
    if is_root() {
        let uid = account_uid(account_id);
        cmd.uid(uid).gid(uid);
    }
    // 自成进程组:`cursor-agent` 会再拉起 `node index.js worker-server` 帮工,
    // 只杀主进程会把帮工留下变孤儿(见 [`CliConv::pgid`])。自成组之后
    // `kill -- -<pgid>` 一次收干净。pgid == 子进程 pid。
    #[cfg(unix)]
    cmd.process_group(0);

    let mut cli = cmd.spawn().map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("cursor-cli: 启动 {} 失败: {e}", cfg.bin.display()),
        )
    })?;
    // 记下进程组(= 子进程 pid,因为上面 process_group(0) 让它自成组),
    // 下面建 CliConv 时填进去 —— kill_procs 靠它才有的可杀(会话淘汰与泵收尾两处都用)。
    let cli_pgid = cli.id();
    // 喂 prompt 进 stdin 并立刻关(EOF = 输入完毕)。
    if let Some(mut stdin) = cli.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let prompt_owned = prompt.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt_owned.as_bytes()).await;
            // drop stdin → EOF
        });
    }

    // CLI 已起:它会按 mcp.json 拉起桥,桥经 socket 回连。等这个回连(10s 上限,
    // CLI 冷启动要初始化 MCP,比裸进程慢)。
    let sock = match listener {
        Some(l) => match tokio::time::timeout(Duration::from_secs(10), l.accept()).await {
            Ok(Ok((s, _))) => Some(s),
            _ => return Err(UpstreamError::new(
                UpstreamErrorKind::Other,
                "cursor-cli 桥 10s 内未回连(MCP server 启动失败?)".to_string(),
            )),
        },
        None => None,
    };

    // peek 尽量靠后:离 commit 越近,代际被别人推进的窗口越小。
    let sim = sim.map(SimRequest::peek);
    let conv = Arc::new(CliConv {
        account_id: account_id.to_string(),
        cli_session_id: Mutex::new(resume_sid),
        fps: Mutex::new(Vec::new()),
        out: OutQueue::new(),
        pending: Mutex::new(None),
        pgid: Mutex::new(cli_pgid),
        at: Mutex::new(Instant::now()),
    });
    convs.insert(conv_key, conv.clone());
    let out = conv.out.clone();
    // 开泵时的已知 token:以 auth.json 现状为准(prepare_home 对账后文件可能比
    // 号库还新);读不到就退号库 token。pump 据此探测 CLI 中途的轮换。
    let auth_file = home.join(".config/cursor/auth.json");
    let known_token = read_auth_creds(&auth_file)
        .map(|(at, _)| at)
        .unwrap_or_default();
    let (first_ctx_base, first_in_tally) = first_phase_basis(
        if resumed {
            sim.as_ref().map(|s| s.sim_total).unwrap_or(0)
        } else {
            0
        },
        system,
        prompt,
        tools,
    );
    // 子进程所有权交给 pump:pump 结束(Done/出错/超时)即 drop,kill_on_drop 收尾。
    tokio::spawn(pump(PumpArgs {
        conv,
        cli,
        sock,
        echo_model: echo_model.to_string(),
        auth_file,
        known_token,
        updates,
        first_in_tally,
        first_ctx_base,
        sim,
    }));
    Ok(Box::pin(drain_stream(out)))
}

/// 喂回桥调用结果(调用方带 tool_result 的下一轮请求),返回继续输出的响应流。
///
/// `results` = 本轮带回的 (tool_use_id, 文本) 列表。消费槽位**按 id 键控**:
/// 没有匹配项就显式报错且保留槽位 —— 把别的轮次/别的会话的 tool_result 喂进
/// CLI 是静默语义损坏,比报错严重得多。
pub fn resume_conv(
    conv: Arc<CliConv>,
    results: Vec<(String, String)>,
    sim: Option<SimRequest>,
) -> Result<gw_core::provider::ChatStream, UpstreamError> {
    conv.touch();
    // 先校验、**后 peek**:错配的 tool_result 根本不算一次有效轮次,让它先把模拟
    // 状态读出来只会白白拉长 peek→commit 的竞态窗口(对抗评审 high#3)。
    let (slot, text) = conv.take_pending_matching(&results)?;
    // 槽随结果一起走通道交给泵 —— 不落会话共享字段,所以并发请求不会互相覆盖,
    // 也不存在"装槽晚于唤醒"的竞态(两者现在是同一次 send,原子)。
    let sim = sim.map(SimRequest::peek);
    slot.responder.send((Ok(text), sim)).map_err(|_| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            "cursor-cli: 泵任务已退出,桥调用无法送达".to_string(),
        )
    })?;
    Ok(Box::pin(drain_stream(conv.out.clone())))
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用:造一个只关心 `cache_read` / `sim_total` 的槽。
    fn test_slot(cache_read: u64, sim_total: u64) -> SimSlot {
        SimSlot {
            key: "k".into(),
            model: "m".into(),
            fps: Vec::new(),
            gen: 0,
            cache_read,
            raw_hit: cache_read,
            sim_total,
        }
    }

    /// 攒字符再算一次 == 把整段文本一次性估算。这是 [`TokenTally`] 存在的理由:
    /// 逐段调 `est_text_tokens` 相加会因为 ASCII 的 `ceil(n/4)` 每段取整而虚高。
    #[test]
    fn 累加器与一次性估算完全一致_且不受分段方式影响() {
        let segs = ["hello ", "世界", "abc", "d", "又一段中文", "!"];
        let whole: String = segs.concat();

        let mut t = TokenTally::default();
        for s in segs {
            t.push(s);
        }
        assert_eq!(
            t.tokens(),
            crate::chat::est_text_tokens(&whole),
            "分段累加必须等于整段估算"
        );

        // 反例:逐段各自估算再相加会虚高(正是这里要避免的)。
        let naive: u64 = segs.iter().map(|s| crate::chat::est_text_tokens(s)).sum();
        assert!(
            naive > t.tokens(),
            "逐段取整应当虚高,否则这个测试就失去意义了(naive={naive}, tally={})",
            t.tokens()
        );
    }

    /// 工具轮次必须报出**非零**用量,而且 output 要含工具参数 —— 这条锁的是
    /// 2026-08-17 那个「56% 请求记 0」的回归(以前这里硬写 0)。
    #[test]
    fn 工具轮次报出自估用量_含工具参数且不为零() {
        let out = OutQueue::new();
        let mut phase = SsePhase::with_input("grok-4.6", TokenTally::default(), None, 0);
        // 纯工具调用轮:一个字正文都没有,产出全在参数 JSON 里。
        phase.finish_tool_use(
            &out,
            "toolu_x",
            "read_file",
            &json!({"path": "/some/rather/long/path/to/a/file.rs", "limit": 200}),
        );

        let mut usage = None;
        let mut delta_usage = None;
        let drained: Vec<OutItem> = out
            .q
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect();
        for item in drained {
            match item {
                OutItem::Item(Ok(StreamItem::Usage(u))) => usage = Some(u),
                OutItem::Item(Ok(StreamItem::Sse(e))) => {
                    if e.data.get("type").and_then(|t| t.as_str()) == Some("message_delta") {
                        delta_usage = e.data.get("usage").cloned();
                    }
                }
                _ => {}
            }
        }

        let usage = usage.expect("工具轮次必须 push StreamItem::Usage,否则落库恒 0");
        assert!(
            usage.output_tokens > 0,
            "纯工具轮的产出大头是参数 JSON,不能估成 0"
        );
        let du = delta_usage.expect("message_delta 必须带 usage");
        assert_eq!(
            du.get("output_tokens").and_then(|v| v.as_u64()),
            Some(usage.output_tokens),
            "SSE 里报给客户的与落库的必须是同一个数"
        );
    }

    /// 正文与 thinking 都要计进 output(上游就是这么收费的)。
    #[test]
    fn 正文与thinking都计入output() {
        let out = OutQueue::new();
        let mut phase = SsePhase::with_input("grok-4.6", TokenTally::default(), None, 0);
        phase.push_text(&out, "thinking", "先想一想这个问题", true);
        let after_thinking = phase.out_tally.tokens();
        assert!(after_thinking > 0, "thinking 必须计入");
        phase.push_text(&out, "text", "然后给出答案", false);
        assert!(
            phase.out_tally.tokens() > after_thinking,
            "正文要在 thinking 之上继续累加"
        );
    }

    /// 系数只乘 output,不乘 input —— input 侧没有隐藏推理,乘了就是凭空多收。
    #[test]
    fn 校正系数只作用于output() {
        let mut t = TokenTally::default();
        t.push("这是一段足够长的中文文本用来让取整误差不至于淹没结论");
        let phase = SsePhase::with_input("grok-4.6", t, None, 0);
        let u = phase.estimated_usage();
        assert_eq!(u.input_tokens, t.tokens(), "input 不得乘系数");
        assert_eq!(u.output_tokens, 0, "本阶段没有产出");
        assert!(TOOL_ROUND_TOKEN_FACTOR > 1.0, "系数应当>1(补隐藏推理)");
    }

    /// 自估轮必须报出模拟缓存,且 `input_tokens` 是**总输入**(新增 + 命中)。
    ///
    /// 锁两件事:
    /// 上游 cache 不可用时必须**回落模拟值**,不能留 0。
    ///
    /// 这条锁的是 2026-08-17 用户报障:「为什么总是有这种突然没有任何缓存命中的」——
    /// 末轮上游报的 cache 是跨内部调用累加值(超过 input),被判不可信丢弃后计 0,
    /// 于是有真值的那一轮反而全价。实测 input=85951 / 上游 162176 被弃 /
    /// 模拟 57336 可用 → 客户多付约 2.4 倍。
    #[test]
    fn 上游缓存不可用时回落模拟值() {
        // 复刻生产样本:上游 cache=162176 > input=85951 → usage_from_result 丢弃成 0。
        let mut u = usage_from_result(&json!({
            "inputTokens": 85951, "outputTokens": 1323, "cacheReadTokens": 162176
        }));
        assert_eq!(u.cache_read_tokens, 0, "前提:不可信的上游 cache 已被丢弃");

        // 同一请求模拟器给出 57336 命中 → 必须顶上。
        let got = fallback_cache_from_sim(&mut u, 57336);
        assert_eq!(got, Some(57336));
        assert_eq!(u.cache_read_tokens, 57336, "计费列要用模拟值,不能留 0");
        assert_eq!(u.input_tokens, 85951, "input 不受回落影响");
        assert_eq!(u.real_cache_read_tokens, 0, "对账列仍不认模拟值");

        // 客户侧因此拿到折扣,而不是整轮全价。
        let wire = crate::chat::delta_usage_json_pub(&u);
        assert_eq!(
            wire.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
            Some(57336)
        );
        assert_eq!(
            wire.get("input_tokens").and_then(|v| v.as_u64()),
            Some(85951 - 57336)
        );
    }

    /// 回落只在"上游没给可用值"时发生,且绝不破 `input ≥ cache_read`。
    #[test]
    fn 回落不覆盖上游真值且夹到input内() {
        // ① 上游已有可用 cache → 不回落(真值优先于模拟)。
        let mut u = usage_from_result(&json!({
            "inputTokens": 6779, "outputTokens": 35, "cacheReadTokens": 6016
        }));
        assert_eq!(fallback_cache_from_sim(&mut u, 99999), None, "有真值就不该回落");
        assert_eq!(u.cache_read_tokens, 6016);

        // ② 模拟值超过 input → 夹到 **input × cap**(默认 0.9),不是 input 本身。
        //    夹到 input 会让客户侧输入归 0 → 整轮按缓存价 0.1× 计
        //    (2026-08-17 实测 63 条里 57 条中招,我方大幅贴钱)。
        let mut u2 = usage_from_result(&json!({
            "inputTokens": 1000, "outputTokens": 10, "cacheReadTokens": 50000
        }));
        assert_eq!(u2.cache_read_tokens, 0);
        let cap = crate::cache_sim::billing().cap_ratio;
        let expect = (1000.0 * cap).round() as u64;
        assert_eq!(fallback_cache_from_sim(&mut u2, 40000), Some(expect));
        assert_eq!(u2.cache_read_tokens, expect, "必须夹到 input × cap");
        assert!(u2.cache_read_tokens < u2.input_tokens, "客户侧输入必须留余量");

        // ③ 模拟值为 0 → 无事发生。
        let mut u3 = usage_from_result(&json!({
            "inputTokens": 1000, "outputTokens": 10, "cacheReadTokens": 9999
        }));
        assert_eq!(fallback_cache_from_sim(&mut u3, 0), None);
        assert_eq!(u3.cache_read_tokens, 0);
    }

    /// 自估轮的 `input_tokens` 基准 = **上游会话已有量 + 本轮新增量**。
    ///
    /// 不能只用本轮新增量(`in_tally`):上游每轮读整个会话文件,不是我方这轮送的
    /// 那几十字节。2026-08-18 生产实测(grok-4.6 同一条会话)——上游真值那轮
    /// `input=121,483`,紧接着 7 个工具轮全部报 `input=10`(喂回的 tool_result 长度),
    /// 客户按 10 个 token 付输入,我方吃掉 12 万 token 的成本。
    ///
    /// 也不能用 `sim_total`:标定 10 条样本中位 0.741(少收 25%),且重铺轮出现
    /// **26.6 倍**离群(超收 27 倍)。累加式两个方向都躲开,见 [`SsePhase::ctx_base`]。
    #[test]
    fn 自估轮的input基准是会话已有量加本轮新增量() {
        let mut t = TokenTally::default();
        t.push("本轮新增的输入文本,长度要够让命中值有意义".repeat(50).as_str());
        let fresh = t.tokens();
        assert!(fresh > 100, "新增量要够大,否则夹限会掩盖基准差异: {fresh}");

        // 上游会话里已经攒了一大段(典型工具轮:前面几轮的输入与输出都在里面)。
        let ctx_base = fresh * 30;
        let basis = ctx_base + fresh;
        // 模拟器按完整上下文算命中,取一个落在基准之内的值。
        let slot = test_slot(basis / 2, basis);
        let u =
            SsePhase::with_input("grok-4.6", t, Some(&slot), ctx_base).estimated_usage();
        assert_eq!(
            u.input_tokens, basis,
            "input 必须是 ctx_base + 本轮新增,只报新增会低估几个数量级"
        );
        assert_eq!(u.cache_read_tokens, basis / 2, "命中在基准之内时原样报");
        assert_eq!(u.real_cache_read_tokens, 0, "模拟值绝不写对账列");

        let wire = crate::chat::delta_usage_json_pub(&u);
        assert_eq!(
            wire.get("input_tokens").and_then(|v| v.as_u64()),
            Some(basis - basis / 2),
            "客户侧 = 基准 − 命中"
        );
    }

    /// 续会话的首阶段基准 = `sim_total` 单独一份,**不得再叠 tools/prompt**。
    ///
    /// 这条锁我自己在 2026-08-18 引入又发现的重复计费:`ctx_base = sim_total` 的同时
    /// 还把 `first_in_tally`(prompt + tools)加了上去 —— 而 `sim_total` 的口径本来就
    /// 含 system + tools + 全部历史。tools 对 Claude Code 一类客户能有上万 token。
    #[test]
    fn 续会话首阶段基准不重复计tools与prompt() {
        let tools = vec![crate::run::ToolDef {
            name: "read_file".into(),
            description: "读一个文件,描述写长一点好让 token 数明显".repeat(20),
            schema: r#"{"type":"object"}"#.repeat(20),
        }];
        let sim_total = 123_456u64;
        let (ctx, tally) =
            first_phase_basis(sim_total, &"很长的 system 提示".repeat(50), "问题", &tools);
        assert_eq!(ctx, sim_total, "续会话基准就是 sim_total 本身");
        assert_eq!(
            tally.tokens(),
            0,
            "in_tally 必须留空,否则 tools/prompt 被算两次(sim_total 里已有)"
        );
    }

    /// 新会话首阶段基准必须**含 system**:AGENTS.md 每轮被 CLI 重读,上游照样计费。
    ///
    /// 2026-08-18 受控实测:新会话我方自估首轮 48,同一进程退出时上游真值 16,821。
    #[test]
    fn 新会话首阶段基准含system() {
        let tools = vec![crate::run::ToolDef {
            name: "t".into(),
            description: "d".into(),
            schema: "{}".into(),
        }];
        let system = "这是一段相当长的 system 提示,客户端通常会塞几千 token 进来".repeat(30);
        let (ctx, with_sys) = first_phase_basis(0, &system, "问题", &tools);
        assert_eq!(ctx, 0, "新会话没有已有上下文");
        let (_, without_sys) = first_phase_basis(0, "", "问题", &tools);
        assert!(
            with_sys.tokens() > without_sys.tokens() + 100,
            "system 必须计进基准: {} vs {}",
            with_sys.tokens(),
            without_sys.tokens()
        );
    }

    /// 新开会话(含**重铺**)的首阶段 `ctx_base` 必须是 0 —— 上游手里没有那段历史。
    ///
    /// 这条锁标定里那个 26.6 倍离群的成因:调用方带回全量历史(模拟器按整段算
    /// 76,890),而我方新开了 CLI 会话、实际只喂 2,887。若首阶段从 `sim_total`
    /// 起算就会超收 27 倍。
    #[test]
    fn 新开会话首阶段基准不含未送上去的历史() {
        let mut t = TokenTally::default();
        t.push("只把最后一段用户输入喂上去".repeat(30).as_str());
        let fresh = t.tokens();
        // sim_total 是调用方全量历史(很大),但上游根本没收到它。
        let slot = test_slot(fresh * 5, fresh * 27);
        let u = SsePhase::with_input("grok-4.6", t, Some(&slot), 0).estimated_usage();
        assert_eq!(u.input_tokens, fresh, "新开会话的基准只能是我方实际喂进去的量");
        let cap = crate::cache_sim::billing().cap_ratio;
        assert_eq!(
            u.cache_read_tokens,
            (fresh as f64 * cap).floor() as u64,
            "命中仍按 cap 夹,不得因为 sim_total 虚高而放行"
        );
    }

    /// 模拟命中超过本轮基准时夹到 **基准 × cap**,客户侧输入**恒为正**。
    ///
    /// 这条锁的是 2026-08-17 部署后实测到的回归:原先夹到 `fresh_in` 本身,于是
    /// `cache == input` → 客户侧输入 0 → **整轮按缓存价 0.1× 计**。63 条里 57 条
    /// (90%)中招,总输入 716,826 中 635,150(88.6%)按 0.1× 走,我方大幅贴钱。
    /// 根因:模拟命中按**完整上下文**算(几万),自估基准只是本轮实际输入(几十~几千)。
    #[test]
    fn 模拟命中超过本轮输入时夹到cap而非全额() {
        let mut t = TokenTally::default();
        t.push("一条不长的 tool_result,但要够长以免取整吃掉余量".repeat(20).as_str());
        let fresh = t.tokens();
        assert!(fresh > 50, "基准要够大才看得出 cap 余量: {fresh}");

        let slot = test_slot(fresh * 40 + 9999, fresh * 60);
        let u = SsePhase::with_input("grok-4.6", t, Some(&slot), 0).estimated_usage();
        assert_eq!(u.input_tokens, fresh);

        let cap = crate::cache_sim::billing().cap_ratio;
        let expect = (fresh as f64 * cap).floor() as u64;
        assert_eq!(u.cache_read_tokens, expect, "命中必须夹到 基准 × cap");
        assert!(u.cache_read_tokens < fresh, "绝不能等于基准(那就是整轮 0.1×)");

        let wire = crate::chat::delta_usage_json_pub(&u);
        let uncached = wire.get("input_tokens").and_then(|v| v.as_u64()).unwrap();
        assert!(uncached > 0, "客户侧输入必须为正,got {uncached}");
        assert_eq!(uncached, fresh - expect);
    }

    /// 客户侧输入**恒为正**:`cap_ratio < 1` 保证命中不会吃掉全部输入。
    ///
    /// 这条锁的是 2026-08-17 线上实测的那个 bug:48 条有缓存记录里 26 条
    /// `cache/input > 0.95`、最高 0.9998,客户侧输入只剩 18 个 token。
    /// 根因是 cursor 通道从未接 kiro 的三参数夹限(`reported_cache_read`)。
    #[test]
    fn 夹限保证客户侧输入恒为正() {
        use crate::cache_sim::{reported_cache_read, CacheBilling};
        // 模拟器"几乎全命中"(99.98%)的极端输入,过夹限后必须留出余量。
        let total = 172_687u64;
        let raw_hit = 172_626u64;
        let b = CacheBilling { read_multiplier: 1.8, cap_ratio: 0.95, floor_ratio: 0.75 };
        let reported = reported_cache_read(total, raw_hit, total, b);
        assert!(
            reported <= (total as f64 * 0.95).round() as u64,
            "上报命中不得超过 cap: {reported} / {total}"
        );
        let uncached = total - reported;
        assert!(
            uncached >= total / 20,
            "客户侧输入至少留 5%(cap=0.95): uncached={uncached} total={total}"
        );
        // 冷启动(0 命中)时 floor 仍会给出下限,但绝不超过 cap。
        let cold = reported_cache_read(total, 0, total, b);
        assert_eq!(cold, (total as f64 * 0.75).round() as u64, "0 命中时按 floor 报");
        // total=0 不 panic。
        assert_eq!(reported_cache_read(0, 0, 0, b), 0);
    }

    /// `inputTokens` 是**总输入**(含缓存),原样填 —— 绝不能再加 cached。
    ///
    /// 判据是实测单次调用样本(见 [`usage_from_result`] 文档):10 字符的提示词报
    /// `input=6779 / cache=6016`,只可能是总量语义。这条锁住"别再改回 up_in+cached":
    /// 那样会给每个正常轮次凭空加一倍输入(我 2026-08-17 犯过,被对抗评审顶回来)。
    #[test]
    fn 上游真值轮的input是总量不得再加缓存() {
        let u = usage_from_result(&json!({
            "inputTokens": 6779, "outputTokens": 35, "cacheReadTokens": 6016
        }));
        assert_eq!(u.input_tokens, 6779, "必须原样填,不得加 cached");
        assert_eq!(u.cache_read_tokens, 6016);
        assert_eq!(u.real_cache_read_tokens, 6016, "上游自报是真实命中,要进对账列");

        // 线缆侧减一次 → 客户看到未命中 763 + 缓存 6016,合计正好是总量 6779。
        let wire = crate::chat::delta_usage_json_pub(&u);
        assert_eq!(wire.get("input_tokens").and_then(|v| v.as_u64()), Some(763));
        assert_eq!(
            wire.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
            Some(6016)
        );
    }

    /// `cache > input`(上游跨内部调用累加,与本轮 input 不同口径)必须**丢弃**该
    /// 缓存值,而**不是**封顶到 input。
    ///
    /// 这条锁的是 2026-08-17 部署后实测到的回归:封顶让 `cache == input`,线缆侧
    /// `input − cache` 于是恒 0 —— 客户面板「输入」列照样是 0(5 条里 3 条被封成全等,
    /// 比封顶前的 39% 更糟),而且 `cache == input` 是"本轮输入 100% 命中"的假断言。
    #[test]
    fn 上游缓存超过输入时丢弃而非封顶() {
        // 生产真实样本 id=1572918:input=72811 / cache=366720(5 倍)。
        let u = usage_from_result(&json!({
            "inputTokens": 72811, "outputTokens": 3940, "cacheReadTokens": 366720
        }));
        assert_eq!(u.input_tokens, 72811, "input 保持上游原值,不去重构总量");
        assert_eq!(u.cache_read_tokens, 0, "不可信的 cache 一律丢弃,不得封顶成 input");
        assert_eq!(u.real_cache_read_tokens, 0, "对账列同样不存不可信的值");

        let wire = crate::chat::delta_usage_json_pub(&u);
        assert_eq!(
            wire.get("input_tokens").and_then(|v| v.as_u64()),
            Some(72811),
            "客户侧输入必须是真实总量 —— 归 0 就是那个「输入 0」的回归"
        );
        assert!(
            wire.get("cache_read_input_tokens").is_none(),
            "丢弃后不该报缓存字段(宁可不给折扣,也不谎报命中)"
        );
    }

    /// 上游没报缓存时口径不变(cached=0 → input 就是上游原值),
    /// 且不会凭空往对账列写数。
    #[test]
    fn 上游无缓存时口径不变() {
        let u = usage_from_result(&json!({
            "inputTokens": 1234, "outputTokens": 56, "cacheReadTokens": 0
        }));
        assert_eq!(u.input_tokens, 1234);
        assert_eq!(u.cache_read_tokens, 0);
        assert_eq!(u.real_cache_read_tokens, 0);
        // 字段缺失也不能 panic,按 0 处理。
        let empty = usage_from_result(&json!({}));
        assert_eq!(
            (empty.input_tokens, empty.output_tokens, empty.cache_read_tokens),
            (0, 0, 0)
        );
    }

    /// 无模拟槽时(sim=None → 0)自估用量与旧行为逐字节一致:不报缓存、
    /// input 就是本轮新增。锁住「模拟只是叠加,不改变基线」。
    #[test]
    fn 无模拟槽时退回旧行为() {
        let mut t = TokenTally::default();
        t.push("abc 中文");
        let fresh = t.tokens();
        let u = SsePhase::with_input("grok-4.6", t, None, 0).estimated_usage();
        assert_eq!(u.cache_read_tokens, 0);
        assert_eq!(u.input_tokens, fresh);
        let wire = crate::chat::delta_usage_json_pub(&u);
        assert!(
            wire.get("cache_read_input_tokens").is_none(),
            "没有命中时不该出现缓存字段"
        );
    }

    /// `kill_procs` 必须把**整组**带走 —— 包括子进程自己拉起的孙进程。
    ///
    /// 这条锁的是 2026-08-17 的双重泄漏:①`kill_procs` 曾是空壳,导致泵卡在
    /// `stderr_task.await` 上永不返回(循环等待);②即便杀了主进程,`cursor-agent`
    /// 拉起的 `node worker-server` 帮工也会活下来变孤儿(容器里 worker 是 PID 1,
    /// 孤儿重挂到它名下但它不回收)。所以这里造一个"父 + 孙"的进程组来验。
    #[cfg(unix)]
    #[test]
    fn kill_procs_连孙进程一起收干净() {
        use std::os::unix::process::CommandExt as _;

        // 父进程 fork 出一个长睡的孙进程,然后自己也长睡 —— 模拟 cursor-agent + 帮工。
        let mut child = unsafe {
            std::process::Command::new("sh")
                .arg("-c")
                .arg("sleep 300 & echo $! ; sleep 300")
                .stdout(std::process::Stdio::piped())
                .pre_exec(|| {
                    // 自成进程组,与生产里 process_group(0) 等价。
                    if libc_setpgid() != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .expect("起测试进程")
        };
        let pgid = child.id();

        // 读出孙进程 pid。
        let grandchild: u32 = {
            use std::io::Read as _;
            let mut s = String::new();
            let mut out = child.stdout.take().expect("stdout piped");
            // 只读第一行就够(sh 立刻 echo,之后才 sleep)。
            let mut buf = [0u8; 64];
            let n = out.read(&mut buf).expect("读孙进程 pid");
            s.push_str(&String::from_utf8_lossy(&buf[..n]));
            s.trim().parse().expect("孙进程 pid 是数字")
        };

        let alive = |pid: u32| std::path::Path::new(&format!("/proc/{pid}")).exists();
        assert!(alive(pgid), "父进程应当在跑");
        assert!(alive(grandchild), "孙进程应当在跑");

        let conv = CliConv {
            account_id: "acc".into(),
            cli_session_id: Mutex::new(None),
            fps: Mutex::new(Vec::new()),
            out: OutQueue::new(),
            pending: Mutex::new(None),
            pgid: Mutex::new(Some(pgid)),
            at: Mutex::new(Instant::now()),
        };
        conv.kill_procs();

        // 组信号是异步送达的,给一点时间。**先判定再收尸** —— 反过来写的话,
        // kill_procs 万一没生效,`child.wait()` 会一直等那 300s 的 sleep,
        // 测试表现为挂死而不是干净的断言失败(实测过,别改回去)。
        let died = (0..100).any(|_| {
            if !alive(grandchild) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
            false
        });

        // 兜底清场:无论断言成不成立,都别把 sleep 进程和僵尸留给别的测试。
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg("--")
            .arg(format!("-{pgid}"))
            .status();
        let _ = child.wait();

        assert!(
            died,
            "孙进程必须跟着死 —— 只杀主进程就是那 33 个孤儿 worker-server 的来源"
        );
    }

    /// `setpgid(0, 0)`:不引 libc crate,直接走 syscall。
    #[cfg(unix)]
    fn libc_setpgid() -> i32 {
        // SAFETY: setpgid(0,0) 无参数指针,pre_exec 里调用是 async-signal-safe 的。
        unsafe { syscall_setpgid() }
    }

    #[cfg(unix)]
    unsafe fn syscall_setpgid() -> i32 {
        unsafe extern "C" {
            fn setpgid(pid: i32, pgid: i32) -> i32;
        }
        unsafe { setpgid(0, 0) }
    }

    /// 造一枚只带 exp 的假 JWT(测试用,不验签)。
    fn fake_jwt(exp: i64) -> String {
        use base64::Engine;
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"exp":{exp}}}"#));
        format!("h.{body}.s")
    }

    fn test_cfg(tag: &str) -> (CliDriverConfig, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "clidrv-test-{}-{}",
            tag,
            uuid::Uuid::new_v4().simple()
        ));
        let cfg = CliDriverConfig {
            bin: PathBuf::from("/bin/true"),
            base_dir: base.clone(),
            self_exe: PathBuf::from("/bin/true"),
        };
        (cfg, base)
    }

    #[test]
    fn account_uid_稳定且在隔离段内() {
        let a1 = account_uid("acc-alpha");
        assert_eq!(a1, account_uid("acc-alpha"), "同账号必须派生同 uid");
        assert!((100_000..500_000).contains(&a1));
        assert_ne!(
            account_uid("acc-alpha"),
            account_uid("acc-beta"),
            "这两个测试账号不该撞 uid(撞了说明哈希空间太小)"
        );
        assert_ne!(account_uid("acc-alpha"), 65534, "不得落回 nobody");
    }

    #[test]
    fn prepare_home_文件更新时上报捕获_且不覆写文件() {
        let (cfg, base) = test_cfg("rotate");
        let updates = TokenUpdates::default();
        let old = fake_jwt(1_000_000);
        let new = fake_jwt(2_000_000);
        // 号库 token 旧;文件里是 CLI 轮换后的新 token。
        prepare_home(&cfg, "acc", "conv-a", &old, None, "", &updates).unwrap();
        write_auth_json(&base.join("acc"), &new, Some("rt-new")).unwrap();

        prepare_home(&cfg, "acc", "conv-a", &old, None, "", &updates).unwrap();

        let captured = updates.lock().unwrap().get("acc").cloned();
        let captured = captured.expect("文件更新必须产生捕获");
        assert_eq!(captured.access_token, new);
        assert_eq!(captured.refresh_token.as_deref(), Some("rt-new"));
        assert!(captured.expires_at.is_some());
        let (at, _) = read_auth_creds(&base.join("acc/.config/cursor/auth.json")).unwrap();
        assert_eq!(at, new, "文件新时不得用号库旧 token 覆写");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn prepare_home_号库更新时覆写文件_且不上报() {
        let (cfg, base) = test_cfg("dbnewer");
        let updates = TokenUpdates::default();
        let old = fake_jwt(1_000_000);
        let new = fake_jwt(2_000_000);
        prepare_home(&cfg, "acc", "conv-a", &old, None, "", &updates).unwrap();

        // gw-app 侧刷新过(号库 exp 更新)→ 文件应被覆写跟上。
        prepare_home(&cfg, "acc", "conv-a", &new, Some("rt-new"), "", &updates).unwrap();

        let (at, rt) = read_auth_creds(&base.join("acc/.config/cursor/auth.json")).unwrap();
        assert_eq!(at, new);
        assert_eq!(rt.as_deref(), Some("rt-new"));
        assert!(updates.lock().unwrap().is_empty(), "号库更新不是轮换,不该上报");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// **2026-08-17 跨会话提示串味的回归锁**:同号两个会话的 AGENTS.md 必须各是各的。
    /// 每号一份时,无工具的角色扮演会话会把它覆成「你没有任何工具可用」,同号那个带
    /// 46 个工具的 Claude Code 会话的 CLI 就读到了那份。
    #[test]
    fn prepare_home_工作区每会话一份_互不覆盖() {
        let (cfg, base) = test_cfg("perconv");
        let updates = TokenUpdates::default();
        let tok = fake_jwt(1_500_000);
        let (_, ws_a) = prepare_home(&cfg, "acc", "conv-a", &tok, None, "甲会话的 system", &updates).unwrap();
        let (_, ws_b) = prepare_home(&cfg, "acc", "conv-b", &tok, None, "乙会话的 system", &updates).unwrap();
        assert_ne!(ws_a, ws_b, "两个会话必须是两个目录");
        assert_eq!(std::fs::read_to_string(ws_a.join("AGENTS.md")).unwrap(), "甲会话的 system");
        assert_eq!(std::fs::read_to_string(ws_b.join("AGENTS.md")).unwrap(), "乙会话的 system");
        // 附件也各自隔离(编号 attach-N 是每请求重排的,共用目录会互相盖图)。
        assert!(ws_a.join("assets").is_dir() && ws_b.join("assets").is_dir());
        // 会话 id 为空时有确定的兜底目录,不会退回共享父目录。
        let (_, ws_n) = prepare_home(&cfg, "acc", "", &tok, None, "x", &updates).unwrap();
        assert_eq!(ws_n, base.join("acc/ws/_noconv"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 迁移:旧版把 AGENTS.md 写在 `ws/` 下,而那是新工作区的**父目录** ——
    /// Cursor 会往上层找 rules,留着等于串味原地保留。必须删掉。
    #[test]
    fn prepare_home_清掉旧的每号共享agents() {
        let (cfg, base) = test_cfg("legacy");
        let updates = TokenUpdates::default();
        let tok = fake_jwt(1_500_000);
        let ws_root = base.join("acc/ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::write(ws_root.join("AGENTS.md"), "别的客户的 system").unwrap();
        prepare_home(&cfg, "acc", "conv-a", &tok, None, "我的 system", &updates).unwrap();
        assert!(!ws_root.join("AGENTS.md").exists(), "旧的共享 AGENTS.md 必须被清掉");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// GC:过期会话目录回收,当前会话与新鲜会话都不能动。
    /// **判据是 `.last` 不是目录 mtime** —— 活会话的目录 mtime 可能很旧。
    #[test]
    fn 工作区gc_只删过期的_不碰当前和新鲜的() {
        let (cfg, base) = test_cfg("wsgc");
        let updates = TokenUpdates::default();
        let tok = fake_jwt(1_500_000);
        let ws_root = base.join("acc/ws");
        // 造三个:过期的、无标记的(旧版残骸)、新鲜的。
        let stale = ws_root.join("conv-stale");
        let nomark = ws_root.join("conv-nomark");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&nomark).unwrap();
        std::fs::write(stale.join(".last"), (now_secs() - WS_TTL.as_secs() - 60).to_string()).unwrap();
        let (_, fresh) = prepare_home(&cfg, "acc", "conv-fresh", &tok, None, "s", &updates).unwrap();
        // 再跑一次别的会话触发 GC(当前会话换成 conv-cur)。
        let (_, cur) = prepare_home(&cfg, "acc", "conv-cur", &tok, None, "s", &updates).unwrap();
        assert!(!stale.exists(), "过期目录该删");
        assert!(!nomark.exists(), "无 .last 标记的旧残骸该删");
        assert!(fresh.exists(), "新鲜会话不能删");
        assert!(cur.exists(), "当前会话不能删");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn prepare_home_同token不动文件() {
        let (cfg, base) = test_cfg("same");
        let updates = TokenUpdates::default();
        let tok = fake_jwt(1_500_000);
        prepare_home(&cfg, "acc", "conv-a", &tok, None, "", &updates).unwrap();
        let file = base.join("acc/.config/cursor/auth.json");
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        prepare_home(&cfg, "acc", "conv-a", &tok, None, "", &updates).unwrap();
        let after = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(before, after, "同 token 不该重写文件(刷 mtime)");
        assert!(updates.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pending_按tool_use_id键控消费() {
        let conv = CliConv {
            account_id: "acc".into(),
            cli_session_id: Mutex::new(None),
            fps: Mutex::new(Vec::new()),
            out: OutQueue::new(),
            pending: Mutex::new(None),
            at: Mutex::new(Instant::now()),
            pgid: Mutex::new(None),
        };
        // 无挂起 → 明确报错。
        assert!(conv
            .take_pending_matching(&[("toolu_x".into(), "t".into())])
            .is_err());

        let (tx, _rx) = oneshot::channel::<(Result<String, String>, Option<SimSlot>)>();
        *conv.pending.lock().unwrap() = Some(PendingSlot {
            tool_use_id: "toolu_abc".into(),
            responder: tx,
        });
        // 错配 → 报错且槽位保留(可带正确结果重试)。
        let err = match conv.take_pending_matching(&[("toolu_other".into(), "别的结果".into())]) {
            Ok(_) => panic!("错配不应成功"),
            Err(e) => e,
        };
        assert!(format!("{err}").contains("toolu_abc"));
        assert!(conv.has_pending(), "错配不得消费槽位");
        // 匹配(即使混在多个结果里)→ 消费成功。
        let (slot, text) = conv
            .take_pending_matching(&[
                ("toolu_other".into(), "别的".into()),
                ("toolu_abc".into(), "正确结果".into()),
            ])
            .expect("匹配应成功");
        assert_eq!(slot.tool_use_id, "toolu_abc");
        assert_eq!(text, "正确结果");
        assert!(!conv.has_pending());
    }
}
