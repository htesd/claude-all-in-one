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

/// 待调用方应答的桥调用。
struct PendingSlot {
    /// 给调用方的 tool_use.id —— 消费槽位时**按它键控**(防错配/重放注入)。
    tool_use_id: String,
    /// 把结果还给泵任务的通道。
    responder: oneshot::Sender<Result<String, String>>,
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
            None => Err(UpstreamError::bad_request_visible(format!(
                "cursor-cli: 带回的 tool_result 与挂起的 tool_use({})不匹配,已拒绝(可带正确结果重试)",
                slot.tool_use_id
            ))),
        }
    }
    fn touch(&self) {
        *self.at.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }
    fn fresh(&self) -> bool {
        self.at.lock().unwrap_or_else(|p| p.into_inner()).elapsed() < SESSION_TTL
    }
    /// 占位:子进程所有权在 pump 里(kill_on_drop 兜底),这里不做显式 kill。
    fn kill_procs(&self) {}
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

struct SsePhase {
    msg_id: String,
    started: bool,
    next_idx: u32,
    open: Option<(u32, &'static str)>,
    model: String,
}

impl SsePhase {
    fn new(model: &str) -> Self {
        Self {
            msg_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            started: false,
            next_idx: 0,
            open: None,
            model: model.to_string(),
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
    fn finish_tool_use(&mut self, out: &OutQueue, tool_use_id: &str, name: &str, args: &Value) {
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
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_delta",
            json!({"type":"message_delta",
                   "delta":{"stop_reason":"tool_use","stop_sequence":null},
                   "usage":{"input_tokens":0,"output_tokens":0}}),
        )))));
        out.push(OutItem::Item(Ok(StreamItem::Sse(SseEvent::new(
            "message_stop",
            json!({"type":"message_stop"}),
        )))));
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
}

/// 泵:读 CLI stdout + 桥 socket,把事件翻译成 SSE 写进 OutQueue。
/// 桥调用处挂起(等下一轮网关请求喂结果),CLI 进程全程存活。
async fn pump(mut a: PumpArgs) {
    let out = a.conv.out.clone();
    let mut phase = SsePhase::new(&a.echo_model);
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

                let (tx_slot, rx_slot) = oneshot::channel::<Result<String, String>>();
                {
                    let mut p = a.conv.pending.lock().unwrap_or_else(|p| p.into_inner());
                    *p = Some(PendingSlot { tool_use_id: tool_use_id.clone(), responder: tx_slot });
                }
                phase.finish_tool_use(&out, &tool_use_id, &name, &args);
                // 新阶段(下一个 Anthropic 响应)重新开始计数。
                phase = SsePhase::new(&a.echo_model);

                // 挂起等调用方结果(带 TTL)。
                let res = tokio::time::timeout(PENDING_TTL, rx_slot).await;
                let reply = match res {
                    Ok(Ok(Ok(text))) => json!({"result": text}),
                    Ok(Ok(Err(err))) => json!({"error": err}),
                    Ok(Err(_)) => json!({"error": "网关注销了这次调用"}),
                    Err(_) => json!({"error": format!("等待调用方 tool_result 超时({}s)", PENDING_TTL.as_secs())}),
                };
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

    // 收尾
    a.conv.kill_procs();
    if let Some(t) = stderr_task {
        let _ = t.await.map(|s| {
            if !s.trim().is_empty() {
                tracing::debug!(stderr = %s.chars().take(300).collect::<String>(), "cursor-cli stderr");
            }
        });
    }

    match outcome {
        Ok(()) => {
            let (ok, usage) = match &state.result {
                Some(r) if r.get("subtype").and_then(|s| s.as_str()) == Some("success") => {
                    let u = r.get("usage").cloned().unwrap_or_default();
                    (
                        true,
                        ChatUsage {
                            input_tokens: u.get("inputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                            output_tokens: u.get("outputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                            cache_read_tokens: u.get("cacheReadTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                            ..Default::default()
                        },
                    )
                }
                other => {
                    tracing::warn!(result = ?other.as_ref().map(|r| r.to_string().chars().take(200).collect::<String>()),
                        "cursor-cli: 未见成功 result 事件");
                    (false, ChatUsage::default())
                }
            };
            if ok {
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

/// 开一条新的 CLI 会话(新 spawn;`lookup` 决定带不带 --resume),返回首阶段响应流。
#[allow(clippy::too_many_arguments)]
pub async fn start_conv(
    cfg: &CliDriverConfig,
    convs: &CliConversations,
    conv_key: &str,
    account_id: &str,
    home: &Path,
    ws: &Path,
    cli_model: &str,
    prompt: &str,
    resume_sid: Option<String>,
    tools: &[crate::run::ToolDef],
    echo_model: &str,
    updates: TokenUpdates,
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
    // 降权:每账号独立 uid 运行(见 account_uid)。ask 模式的只读 shell 仍会真跑
    // 命令 —— 不能让模型用 `cat` 读走 /app/data 的号库,也不能让它读走**别号**
    // 的 HOME(共用 nobody 时 700/600 不挡同 uid)。仅在 root(容器)下启用;
    // 本机开发自动跳过(非 root setuid 会 EPERM)。
    #[cfg(unix)]
    if is_root() {
        let uid = account_uid(account_id);
        cmd.uid(uid).gid(uid);
    }

    let mut cli = cmd.spawn().map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("cursor-cli: 启动 {} 失败: {e}", cfg.bin.display()),
        )
    })?;
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

    let conv = Arc::new(CliConv {
        account_id: account_id.to_string(),
        cli_session_id: Mutex::new(resume_sid),
        fps: Mutex::new(Vec::new()),
        out: OutQueue::new(),
        pending: Mutex::new(None),
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
    // 子进程所有权交给 pump:pump 结束(Done/出错/超时)即 drop,kill_on_drop 收尾。
    tokio::spawn(pump(PumpArgs {
        conv,
        cli,
        sock,
        echo_model: echo_model.to_string(),
        auth_file,
        known_token,
        updates,
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
) -> Result<gw_core::provider::ChatStream, UpstreamError> {
    conv.touch();
    let (slot, text) = conv.take_pending_matching(&results)?;
    slot.responder.send(Ok(text)).map_err(|_| {
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
        };
        // 无挂起 → 明确报错。
        assert!(conv
            .take_pending_matching(&[("toolu_x".into(), "t".into())])
            .is_err());

        let (tx, _rx) = oneshot::channel::<Result<String, String>>();
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
