//! wire v2 探针:验证「官方新方言 + 描述符回放」在**我方合成流量**上是否成立。
//!
//! ```bash
//! cargo run -p gw-cursor --example probe_wire_v2
//! ```
//!
//! ## 它要回答的六件事(按重要性)
//!
//! 1. **`4.2` 内容分节哈希回显有没有出现** —— 整个方案唯一的地基风险。PROTOCOL §20.2
//!    记着我方合成的续轮曾被服务端用 `4.2={1:bin[32]}` 卡住(内容寻址存储要客户端
//!    补传分节);而 08-23 观察官方 resume 时零上传。两件事不矛盾但**不能互推**:
//!    官方首轮 Run 期间内容已在服务端落好。我方回放时服务端手里有没有那些分节,
//!    没有任何证据。出现即说明描述符回放缺一条 CAS 上传腿。
//! 2. **服务端持史对我方是否生效** —— 靠暗号:turn1 让它记,turn2 问它。
//!    只看「有 `.3`、有 cacheRead」会漏掉「接受了描述符但没真接上历史」那一态
//!    (静默失忆),那正是最贵的误判。
//! 3. **描述符是否逐轮产出**(顶层 `.3` 与 blob 引用个数的增长)。
//! 4. **cacheRead 趋势**(≥3 轮才看得出;官方 resume 冷起 38% 与热续不可比)。
//! 5. **回退检测器是否真响** —— 负面对照:把描述符改一个字节回放。失败形态是
//!    200 + 心跳 + 永不 trailer,**不是** 4xx,所以这里必须验「不挂死」。
//! 6. 帧0/响应体量,给 go/no-go 用。
//!
//! ## 安全与预算
//!
//! - 凭证读 `~/.config/cursor/auth.json`(mode 600),**只打印长度,绝不打印 token**。
//! - 请求数**代码里硬上限 [`MAX_REQUESTS`]**,到了就停(cursor 上游是真额度)。
//! - 直连,不走代理。
//! - 每轮有总期限与停滞期限:裸态必须在 [`STALL_TIMEOUT`] 内被判定,不能挂到超时。

use std::time::{Duration, Instant};

use futures::StreamExt;
use gw_cursor::{cli, run, wire, wirev2};

/// lan 本机直连实测可达。
const HOST: &str = "agentn.global.api5.cursor.sh";

/// 请求数硬上限:3 正 + 1 负 + 1 余量。**改大之前先问用户**。
const MAX_REQUESTS: usize = 5;

/// 单轮总期限。
const TURN_DEADLINE: Duration = Duration::from_secs(120);

/// 停滞期限:这么久没有新帧就判定本轮没有进展(裸态靠它终止,而不是靠总期限)。
const STALL_TIMEOUT: Duration = Duration::from_secs(25);

/// 探针不注入系统提示 —— 尽量贴近官方 CLI 的默认形态,少一个可区分维度。
const SYSTEM: &str = "";

const TIMEZONE: &str = "Asia/Shanghai";
const CWD: &str = "/";
const MODEL: &str = "default";

/// 暗号:turn1 让模型记,turn2 问回来。用它排除「接受描述符但静默失忆」。
const PASSPHRASE: &str = "蓝莓42";

/// 请求预算闸门。计数在**发请求之前**加,失败的请求也算 —— 上游可能已经处理了。
struct Budget {
    used: usize,
}

impl Budget {
    fn new() -> Self {
        Self { used: 0 }
    }

    /// 领一个请求额度。返回 `false` 表示预算用尽,调用方必须停。
    fn take(&mut self, label: &str) -> bool {
        if self.used >= MAX_REQUESTS {
            eprintln!(
                "[预算] 已用 {}/{MAX_REQUESTS},拒绝「{label}」并停止",
                self.used
            );
            return false;
        }
        self.used += 1;
        eprintln!("[预算] {}/{MAX_REQUESTS} → {label}", self.used);
        true
    }
}

/// 一轮的观测结果。字段与 `run::WireTurnOutcome` 对齐,便于直接判决。
#[derive(Default)]
struct TurnReport {
    label: String,
    frame0_bytes: usize,
    req_frames: usize,
    resp_frames: usize,
    /// 本轮尾部捕到的续轮描述符(下一轮回放的料)。
    desc: Option<Vec<u8>>,
    desc_refs: usize,
    usage: Option<run::WireUsage>,
    /// 服务端点名的分节哈希 —— **纯展示用**。判据用下面 driver 的计数。
    demanded_hashes: Vec<[u8; 32]>,
    /// 被点名的分节数(取自 `WireDriver`,与生产同一份计数)。
    demanded: usize,
    /// 其中我方**没供出去**的。这才是失败信号。
    unavailable: usize,
    text: String,
    saw_finish: bool,
    /// 因停滞而中止(裸态的判据之一)。
    stalled: bool,
    /// HTTP 状态码非 200 时记下来。
    http_status: Option<u16>,
    /// 传输层失败(连不上 / 超时 / 非 2xx / 读流错)。
    ///
    /// **必须与协议层的裸态分开**:HTTP 失败也「没出字、没收尾」,按 `WireTurnOutcome`
    /// 算同样是 `Barren`,但那说明的是网络/凭证问题,不是「服务端不认我方描述符」。
    /// 混在一起会让一次 401 被报成「方案不可行」。
    transport_error: Option<String>,
    elapsed_ms: u128,
}

impl TurnReport {
    fn outcome(&self) -> run::WireTurnOutcome {
        run::WireTurnOutcome {
            saw_descriptor: self.desc.is_some(),
            saw_finish: self.saw_finish,
            content_chars: self.text.chars().count(),
            // ⭐ 真实供给结果,不再硬编码「一律拿不出来」(codex 终审 #4)。
            demanded: self.demanded,
            unavailable: self.unavailable,
        }
    }
}

/// 读本机 lan 凭证。**只回 token 本体,调用方不得打印它。**
///
/// 与 `gw-kiro` 的 `probe_version.rs` 同一模式:缺文件就退出而不是 panic ——
/// 探针是手动工具,缺凭证是「没配」不是「坏了」。
fn read_token() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = format!("{home}/.config/cursor/auth.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("缺少凭证 {path}: {e};跳过探针。");
            return None;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("凭证 JSON 解析失败: {e}");
            return None;
        }
    };
    let tok = v["accessToken"].as_str().unwrap_or("").to_string();
    if tok.is_empty() {
        eprintln!("凭证缺 accessToken");
        return None;
    }
    // 只报长度。token 是 JWT,打出来等于把号交出去。
    eprintln!("[凭证] {path} accessToken len={}", tok.len());
    Some(tok)
}

/// 直连 client(不走代理)。
///
/// Cursor 的流式端点**强制 HTTP/2**(降级 h1 会被 ALB 回 464),TLS ALPN 会自动
/// 协商到 h2;`no_proxy()` 是显式的 —— 环境里可能有 `HTTPS_PROXY`,而探针要的是直连。
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        // 只给「等响应头」设期限;流式 body 的读取靠 STALL_TIMEOUT 看门狗管,
        // 总超时会把长回复腰斩(生产同一套理由,见 chat.rs 的注释)。
        .connect_timeout(Duration::from_secs(20))
        .build()
        .expect("build reqwest client")
}

/// 跑一轮。`phase` 决定 `1.1` 是空(首轮/重铺)还是描述符回放(续轮)。
async fn run_turn(
    client: &reqwest::Client,
    token: &str,
    conversation_id: &str,
    sections: &std::sync::Arc<gw_cursor::ContentSections>,
    label: &str,
    prompt: &str,
    phase: cli::CliTurn<'_>,
) -> TurnReport {
    let mut rep = TurnReport {
        label: label.to_string(),
        ..Default::default()
    };
    let started = Instant::now();

    // turn_id 同时是帧0 的 `1.25` 与 x-request-id / x-original-request-id。
    // 服务端校验报文自洽性(stale uuid 会进裸态),所以这三处必须同源。
    let turn_id = uuid::Uuid::new_v4().to_string();
    let model = run::Model::new(MODEL);
    let catalog = cli::cli_catalog_lan();

    let frame0 = cli::build_frame0_cli(prompt, &model, &catalog, conversation_id, &turn_id, phase);
    let context = cli::build_context_frame_cli(SYSTEM, token, conversation_id, TIMEZONE, CWD);
    rep.frame0_bytes = frame0.len();

    // ── 反应式发送:帧0 单发,其余交给 WireDriver ────────────────────────────
    //
    // ⚠️ **与生产共用 `wirev2::WireDriver`**(codex 终审 #4)。早先探针自己拼完 13 帧
    // 一次性 half-close、并把所有点名硬编码成「拿不出来」,与生产的反应式路径是两套
    // 实现 —— 那样测出来的结论不可迁移:既验不了内容槽真能上传,还会把生产其实供得出
    // 的点名误报成失败。帧的**构造**留在这里(负控要能递坏描述符),发送时序共用。
    let payloads = cli::cli_request_frames(&frame0, &context);
    let mut framed: Vec<Vec<u8>> = Vec::with_capacity(payloads.len());
    for (p, compress) in &payloads {
        framed.push(if *compress {
            wire::frame_compressed(p).expect("gzip 请求帧")
        } else {
            wire::frame(p)
        });
    }
    drive_request(
        client,
        token,
        conversation_id,
        &turn_id,
        sections,
        label,
        framed,
        started,
        rep,
    )
    .await
}

/// 发送 + 读流:**模板模式与正常模式共用这一段**。
///
/// 帧已经分好(第 0 帧单发,其余交 driver 推迟),所以模板回放走的是与正常探针
/// 完全相同的 reqwest 通道、相同的反应式帧序、相同的停滞闸 —— 这正是模板模式
/// 要排除传输侧差异的前提(fresh2 当时走 curl)。
#[allow(clippy::too_many_arguments)]
async fn drive_request(
    client: &reqwest::Client,
    token: &str,
    conversation_id: &str,
    turn_id: &str,
    sections: &std::sync::Arc<gw_cursor::ContentSections>,
    label: &str,
    mut framed: Vec<Vec<u8>>,
    started: Instant,
    mut rep: TurnReport,
) -> TurnReport {
    rep.req_frames = framed.len();
    let deferred = framed.split_off(1);
    let (btx, brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
    // 帧0 先进通道;发送端在读完响应前一直不 drop,请求流不 half-close。
    let _ = btx.try_send(Ok(bytes::Bytes::from(framed.remove(0))));
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(brx));

    let url = format!("https://{HOST}/agent.v1.AgentService/Run");
    let rb = cli::cli_headers(token, conversation_id, &turn_id)
        .into_iter()
        .fold(client.post(&url), |rb, (k, v)| rb.header(k, v));

    // `send()` 也要受期限保护:TCP/TLS 通了、h2 流开了,但上游永不发响应头时
    // `send()` 会一直等 —— 停滞看门狗在读流循环里,拿不到响应根本进不去。
    let resp = match tokio::time::timeout(TURN_DEADLINE, rb.body(body).send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            rep.transport_error = Some(format!("请求失败: {e}"));
            eprintln!("    [{label}] 请求失败: {e}");
            rep.elapsed_ms = started.elapsed().as_millis();
            return rep;
        }
        Err(_) => {
            rep.transport_error = Some(format!("等响应头超过 {TURN_DEADLINE:?}"));
            eprintln!("    [{label}] 等响应头超时");
            rep.elapsed_ms = started.elapsed().as_millis();
            return rep;
        }
    };
    rep.http_status = Some(resp.status().as_u16());
    if !resp.status().is_success() {
        rep.transport_error = Some(format!("HTTP {}", resp.status()));
        eprintln!("    [{label}] HTTP {}", resp.status());
        rep.elapsed_ms = started.elapsed().as_millis();
        return rep;
    }

    // ⚠️ **driver 在响应头返回之后才构造**(codex 四轮 #5)。
    //
    // 它的 `deferred_deadline()` 是从构造时刻起算的。若在发请求之前就构造,响应头本身
    // 慢于 3s 时探针会立刻走兜底抢发 ENV,而生产是在响应头返回后才起算、那时还在等 ——
    // 两边时序不同源,探针测出来的帧序就不是生产的帧序。
    let mut driver = wirev2::WireDriver::new(deferred, sections.clone());

    // ── 读流。裸态(200 + 心跳 + 永不 trailer)靠停滞看门狗终止 ───────────────
    let mut stream = resp.bytes_stream();
    let mut decoder = wire::FrameDecoder::new();
    let mut last_progress = Instant::now();
    let sink = Some(&btx);
    // 与生产的 `draining` 同义:turn_commit 之后为真,喂给共用的接受判定。
    let mut probe_draining = false;

    loop {
        if started.elapsed() > TURN_DEADLINE {
            rep.stalled = true;
            eprintln!("    [{label}] 触顶 TURN_DEADLINE,中止");
            break;
        }
        let chunk = tokio::select! {
            biased;
            // 推迟帧兜底期限:独立分支。只在收到帧时检查是不够的 —— ack 之后上游
            // 静默就没有帧来触发检查,ENV 会一路拖到停滞闸(生产同一处修法)。
            _ = tokio::time::sleep_until(driver.deferred_deadline().into()),
                if driver.has_deferred() => {
                driver.flush_deferred(sink, "兜底期限").await;
                last_progress = Instant::now();
                continue;
            }
            r = tokio::time::timeout(
                STALL_TIMEOUT.saturating_sub(last_progress.elapsed()),
                stream.next(),
            ) => r,
        };
        let chunk = match chunk {
            Ok(None) => break, // 流自然结束
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(e))) => {
                rep.transport_error = Some(format!("读流失败: {e}"));
                eprintln!("    [{label}] 读流失败: {e}");
                break;
            }
            Err(_) => {
                rep.stalled = true;
                eprintln!("    [{label}] {STALL_TIMEOUT:?} 无新字节,判定停滞(疑心跳裸态)");
                break;
            }
        };
        decoder.feed(&chunk);
        while let Some((flag, raw)) = decoder.next_frame() {
            let payload = match wire::frame_payload(flag, &raw) {
                Ok(p) => p,
                Err(e) => {
                    // ⚠️ 解压失败必须记进排除链(codex 四轮 #6):静默 continue 的话,
                    // 一条坏帧会让本轮"没出字、没收尾"→ Barren → 负控假报"判据触发"。
                    rep.transport_error = Some(format!("帧解压失败: {e}"));
                    eprintln!("    [{label}] 帧解压失败: {e}");
                    continue;
                }
            };
            rep.resp_frames += 1;

            // 观测:点名的哈希留一份用于打印(纯展示,判据用 driver 的计数)。
            let demands_here = run::content_hash_demands(&payload);
            rep.demanded_hashes.extend(demands_here.iter().copied());

            // 顶层 `.3` = 下一轮回放的料。
            //
            // ⚠️ **与生产同一条接受判定**(codex 四轮 #4):只在 turn_commit 同帧或
            // 之后才作数。原先「任何帧见 `.3` 就覆盖」会把生产明确拒绝的 commit 前
            // 中间态拿去回放,两边规则不同,探针结论就不可迁移。
            let commit = run::is_turn_commit(&payload);
            let mut saw_desc_here = false;
            if let Some(d3) = run::descriptor_field3(&payload) {
                if wirev2::descriptor_acceptable(probe_draining, commit) {
                    rep.desc_refs = run::descriptor_ref_count(d3);
                    rep.desc = Some(d3.to_vec());
                    saw_desc_here = true;
                } else {
                    eprintln!("    [{label}] 忽略 commit 前的中间态 .3({} B)", d3.len());
                }
            }

            // ⭐ 与生产同一份实现:存回显 / 应答点名 / 按登记通知发推迟帧。
            let eff = driver.on_frame(&payload, sink).await;

            let fr = run::parse_frame(&payload);
            rep.text.push_str(&fr.text);
            if let Some(u) = fr.usage {
                rep.usage = Some(u);
                rep.saw_finish = true;
            }
            if commit {
                rep.saw_finish = true;
                probe_draining = true; // 与生产同序:desc_acceptable 用完才置位
                                       // 与生产同源:排水段的供给结果不进本轮 verdict(codex 五轮 #1)。
                driver.set_draining();
            }

            // **心跳不算进展**:每 10s 一个 4 字节心跳,算进展的话 25s 停滞闸永不响,
            // 裸态就从「25 秒判定」退化成「挂到 TURN_DEADLINE」。
            let meaningful = !fr.text.is_empty()
                || !fr.thinking.is_empty()
                || fr.usage.is_some()
                || commit
                || saw_desc_here
                || eff.demanded > 0
                || eff.sections_stored > 0
                || eff.deferred_sent > 0;
            if meaningful {
                last_progress = Instant::now();
            }
        }
    }

    rep.demanded = driver.demanded();
    rep.unavailable = driver.unavailable();

    // ⚠️ **尾流不干净就作废本轮描述符**(codex 五轮 #3),与生产同源:
    // 生产只在「trailer / 用量帧 / turn_commit 后排水段排完」这三种干净收尾才提交,
    // 排水段停滞/读错/解压错一律不提交并作废旧料(见 chat.rs 的 shadow_committable)。
    // 探针原先捕到 `.3` 就一直留着供后续轮回放 —— 那等于回放一份生产会拒绝的描述符,
    // 后续轮的结论就不是生产会有的结论。
    if rep.desc.is_some() && !(rep.saw_finish && !rep.stalled && rep.transport_error.is_none()) {
        eprintln!(
            "    [{label}] 尾流不干净(收尾={} 停滞={} 传输错={:?}),作废本轮描述符",
            rep.saw_finish, rep.stalled, rep.transport_error
        );
        rep.desc = None;
    }

    rep.elapsed_ms = started.elapsed().as_millis();
    rep
}

/// 把描述符的**第一个 32B 内容引用**(`.1`)换成全随机值。
///
/// 与「改 tz 尾字节」测的是**不同**的失败模式,两个都要做:
/// - 改 tz 尾字节:结构合法、内容不自洽 → 测服务端**认不认**这份清单;
/// - 换随机 ref:清单结构完好,但指向一个**根本不存在的内容节** → 测服务端会不会
///   按 `4.2` 点名索取,以及我方拿不出来时判据是否触发。
///
/// 只有后者能覆盖 `ContentUnavailable` 那条路径。返回 `None` = 描述符不是预期形状
/// (首字段不是 `0a 20`),那就如实跳过而不是硬改字节。
fn corrupt_first_ref(desc: &[u8]) -> Option<Vec<u8>> {
    if desc.len() < 34 || desc[0] != 0x0a || desc[1] != 0x20 {
        return None;
    }
    let mut out = desc.to_vec();
    let mut rnd = [0u8; 32];
    getrandom::fill(&mut rnd).ok()?;
    out[2..34].copy_from_slice(&rnd);
    Some(out)
}

/// 一个负面对照的结论。
enum NegResult {
    /// 判据触发,应回退重铺 —— 负控有效。
    Fired(&'static str),
    /// 停滞了但判据没触发:判据漏了这一形态,要补。
    StalledOnly(&'static str),
    /// 被当成正常轮:坏描述符没被识别,最危险。
    Missed(&'static str),
    /// **传输故障,这次负控无效** —— 既不算触发也不算未触发。
    Invalid(&'static str, String),
}

/// 负控判定。**先排除传输故障**(codex 终审 #5)。
///
/// 401 / 超时 / 首帧前读错同样造成「没出字、没收尾」,按 `WireTurnOutcome` 算就是
/// `Barren` → `should_fallback()` 为真 → 会被打印成「✔ 判据触发」。那是假阳性:
/// 它证明的是网络坏了,不是我方判据能识别坏描述符。
///
/// 这条守卫比看起来重要:happy path 上 turn2/turn3 的点名几乎必然被 turn1 的回显
/// 供上(节库跨轮共享),所以**唯一能触达 `ContentUnavailable` 的就是负控②**。
/// 那一轮一旦被传输故障顶成假阳性,我们就失去了「拿不出来这条路真的会响」的全部证据。
fn neg_verdict(name: &'static str, rep: &TurnReport) -> NegResult {
    if let Some(e) = &rep.transport_error {
        return NegResult::Invalid(name, e.clone());
    }
    if rep.outcome().should_fallback() {
        NegResult::Fired(name)
    } else if rep.stalled {
        NegResult::StalledOnly(name)
    } else {
        NegResult::Missed(name)
    }
}

/// 紧凑一行 + 判决。给 go/no-go 用,别加装饰。
fn print_report(rep: &TurnReport) {
    let u = rep.usage.unwrap_or_default();
    let hit = if u.input > 0 {
        format!("{:.0}%", u.cache_read as f64 / u.input as f64 * 100.0)
    } else {
        "n/a".into()
    };
    println!(
        "{:<10} f0={:<5}B req={:<2} resp={:<3} .3={:<3} refs={:<2} \
         in={:<6} out={:<5} cacheR={:<6} cacheW={:<5} 命中={:<5} 4.2={:<2} {:>6}ms",
        rep.label,
        rep.frame0_bytes,
        rep.req_frames,
        rep.resp_frames,
        if rep.desc.is_some() { "有" } else { "无" },
        rep.desc_refs,
        u.input,
        u.output,
        u.cache_read,
        u.cache_write,
        hit,
        rep.demanded,
        rep.elapsed_ms,
    );
    println!(
        "           判决={:?} 收尾={} 停滞={} 出字={}字",
        rep.outcome().verdict(),
        rep.saw_finish,
        rep.stalled,
        rep.text.chars().count()
    );
    if !rep.text.is_empty() {
        let t: String = rep.text.chars().take(80).collect();
        println!("           回答: {}", t.replace('\n', " "));
    }
    if rep.demanded > 0 {
        println!(
            "           上游点名 {} 个内容分节(4.2),我方供出 {} / 拿不出 {}。",
            rep.demanded,
            rep.demanded - rep.unavailable,
            rep.unavailable
        );
        println!("              点名本身是正常协议事件(缓存过期后按需索取);拿不出来才是失败。");
        for h in rep.demanded_hashes.iter().take(4) {
            let hex: String = h[..8].iter().map(|b| format!("{b:02x}")).collect();
            println!("              要 {hex}…");
        }
    }
}

#[tokio::main]
async fn main() {
    // 模式分发:
    //   (无参)         正常探针(3 正 + 2 负,最多 5 个请求)
    //   template-check  模板离线自检,**不发请求**
    //   template        模板回放,发 1 个请求
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "template-check" || mode == "template" {
        let t = match load_template() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("加载模板失败: {e}");
                return;
            }
        };
        let ok = template_selfcheck(&t);
        if mode == "template-check" {
            return;
        }
        if !ok {
            eprintln!("自检未通过,拒绝发送(别浪费预算)。");
            return;
        }
        // 模板回放:**1 个请求**。与正常探针同一条 reqwest 通道与反应式帧序。
        let Some(token) = read_token() else { return };
        let client = build_client();
        let sections = std::sync::Arc::new(gw_cursor::ContentSections::default());
        sections.load_model_library(MODEL);
        let mut budget = Budget::new();
        if !budget.take("模板回放") {
            return;
        }
        eprintln!("[模板模式] conversation_id={} (实物帧回放)", t.conv);
        let rep = TurnReport {
            label: "template".into(),
            frame0_bytes: t.frame0_payload.len(),
            ..Default::default()
        };
        let out = drive_request(
            &client,
            &token,
            &t.conv,
            &t.turn,
            &sections,
            "template",
            t.frames.clone(),
            Instant::now(),
            rep,
        )
        .await;
        print_report(&out);
        println!();
        println!("=== 模板结论 ===");
        println!(
            "顶层 .3 = {}   收尾 = {}   4.2 点名 = {}   传输错 = {:?}",
            if out.desc.is_some() { "有" } else { "无" },
            out.saw_finish,
            out.demanded,
            out.transport_error
        );
        if out.desc.is_some() {
            println!("→ 模板通过:门控在我方构造里。下一步逐块做减法定位最小必需集。");
        } else if out.transport_error.is_some() {
            println!("→ 传输故障,本次无结论(不计入判断)。");
        } else {
            println!("→ 模板也裸态:门控**不在帧内容里**。下一步比对出站字节与头部时序。");
        }
        return;
    }

    let Some(token) = read_token() else { return };
    let client = build_client();
    let mut budget = Budget::new();
    // 共享内容分节库:turn1 的回显要能供 turn2/turn3 的点名(这正是官方 store.db
    // 干的事)。**跨轮共享**,否则每轮都从零开始,点名必然拿不出来。
    let sections = std::sync::Arc::new(gw_cursor::ContentSections::default());
    let loaded = sections.load_model_library(MODEL);
    eprintln!("[节库] model={MODEL} 系统提示节装入={loaded}");
    // 全新会话 uuid:把新方言接到旧方言建的历史上,是服务端视角里不可能存在的状态。
    let conv = uuid::Uuid::new_v4().to_string();
    eprintln!("[会话] conversation_id={conv} host={HOST} model={MODEL}");
    println!();

    // ── 正 1:首轮。1.1 空,让模型记暗号 ──────────────────────────────────────
    if !budget.take("turn1 首轮") {
        return;
    }
    let t1 = run_turn(
        &client,
        &token,
        &conv,
        &sections,
        "turn1",
        &format!("记住暗号:{PASSPHRASE}。只回复「收到」两个字"),
        cli::CliTurn::Opening,
    )
    .await;
    print_report(&t1);
    let Some(d1) = t1.desc.clone() else {
        println!("\n[STOP] 首轮没捕到顶层 .3 —— 没有可回放的描述符,续轮无从谈起。");
        println!("       先查:是不是根本没到收尾(判决/停滞),或响应形态变了。");
        return;
    };

    // ── 正 2:续轮回放。问暗号 —— 这一问是为了排除「静默失忆」那一态 ──────────
    if !budget.take("turn2 续轮回放") {
        return;
    }
    let t2 = run_turn(
        &client,
        &token,
        &conv,
        &sections,
        "turn2",
        "暗号是什么?只回答暗号本身",
        cli::CliTurn::Continuation(&d1),
    )
    .await;
    print_report(&t2);
    let remembered = t2.text.contains(PASSPHRASE);
    println!(
        "           持史={}",
        if remembered {
            "✔ 答出暗号,服务端持史对我方生效"
        } else {
            "✘ 没答出暗号 —— 接受了描述符但历史没接上(静默失忆)"
        }
    );

    // ── 正 3:再续一轮,看 cacheRead 趋势(2 轮看不出趋势,所以要第 3 轮)────────
    let d2 = t2.desc.clone();
    let mut t3: Option<TurnReport> = None;
    if let Some(d2) = d2.as_deref() {
        if budget.take("turn3 续轮回放") {
            let r = run_turn(
                &client,
                &token,
                &conv,
                &sections,
                "turn3",
                "把暗号里的两个汉字倒过来写",
                cli::CliTurn::Continuation(d2),
            )
            .await;
            print_report(&r);
            t3 = Some(r);
        }
    } else {
        println!("\n[跳过 turn3] turn2 没捕到新 .3,没有可回放的料。");
    }

    // ── 负面对照:两种坏描述符各跑一次 ────────────────────────────────────────
    //
    // 失败形态是 200 + 心跳 + 无 trailer,等 4xx 等不到 —— 这两轮就是为了证明我方
    // 判据不依赖错误码。哪个先触发就用哪个的结论;**都不触发就如实报「负控未触发」**,
    // 不许拿"没炸"当"判据可用"。
    let mut neg_results: Vec<NegResult> = Vec::new();
    if let Some(d2) = d2.as_deref() {
        // 负控①:改 tz 尾字节 —— 结构合法、内容不自洽。
        if budget.take("负控① tz 尾字节") {
            let mut bad = d2.to_vec();
            if let Some(last) = bad.last_mut() {
                *last ^= 0x01;
            }
            let neg = run_turn(
                &client,
                &token,
                &conv,
                &sections,
                "neg-tz",
                "这一轮预期不该正常完成",
                cli::CliTurn::Continuation(&bad),
            )
            .await;
            print_report(&neg);
            neg_results.push(neg_verdict("tz 尾字节", &neg));
        }
        // 负控②:第一个内容引用换随机 —— 清单指向不存在的节,测 4.2 + 拿不出来。
        match corrupt_first_ref(d2) {
            Some(bad) => {
                if budget.take("负控② 随机 ref") {
                    let neg = run_turn(
                        &client,
                        &token,
                        &conv,
                        &sections,
                        "neg-ref",
                        "这一轮预期不该正常完成",
                        cli::CliTurn::Continuation(&bad),
                    )
                    .await;
                    print_report(&neg);
                    neg_results.push(neg_verdict("随机 ref", &neg));
                }
            }
            None => println!("\n[跳过负控②] 描述符不是预期形状(首字段非 `0a 20`),不硬改字节。"),
        }
    }

    // ── 汇总 ────────────────────────────────────────────────────────────────
    println!("\n=== go/no-go ===");
    let demand_total = t1.demanded + t2.demanded + t3.as_ref().map_or(0, |r| r.demanded);
    let unavail_total = t1.unavailable + t2.unavailable + t3.as_ref().map_or(0, |r| r.unavailable);
    println!(
        "4.2 点名合计 = {demand_total},其中拿不出 = {unavail_total}  →  {}",
        if unavail_total > 0 {
            "NO-GO:有点名供不出来(描述符清单指向我方未持有的内容)"
        } else if demand_total > 0 {
            "GO:被点名且全部供出(上传腿实测可用)"
        } else {
            "本次未被点名 —— ⚠️ 未覆盖上传腿,不能证明长期可行,见下"
        }
    );
    println!(
        "  ⚠️ 口径:官方实测 turn1/turn2 零点名、turn4(距首轮 2.4h 的 resume)点名 7 个。\n\
         \x20    所以短会话不被点名是常态,**探针跑通只覆盖「服务端缓存尚热」这一段**。\n\
         \x20    缓存过期后必被点名,而每份描述符的第一个引用都是 Cursor 那 2025B\n\
         \x20    Composer 系统提示节(不在任何 ENV 帧里,官方 CLI 自持),我方无法自建。"
    );
    println!(
        "服务端持史 = {}",
        if remembered {
            "GO"
        } else {
            "NO-GO:静默失忆"
        }
    );
    let trend: Vec<String> = [Some(&t1), Some(&t2), t3.as_ref()]
        .iter()
        .flatten()
        .map(|r| {
            let u = r.usage.unwrap_or_default();
            if u.input > 0 {
                format!("{:.0}%", u.cache_read as f64 / u.input as f64 * 100.0)
            } else {
                "n/a".into()
            }
        })
        .collect();
    println!("cacheRead 命中趋势 = {}", trend.join(" → "));
    if neg_results.is_empty() {
        println!("负控 = 未跑(没有可用描述符)");
    } else {
        for r in &neg_results {
            match r {
                NegResult::Fired(n) => println!("负控[{n}] = ✔ 判据触发,应回退重铺"),
                NegResult::StalledOnly(n) => {
                    println!("负控[{n}] = △ 停滞但判据没触发 —— 判据漏了这一形态,要补")
                }
                NegResult::Missed(n) => {
                    println!("负控[{n}] = ✘ 被当成正常轮 —— 坏描述符没被识别(最危险)")
                }
                NegResult::Invalid(n, e) => {
                    println!("负控[{n}] = 无效:传输故障({e})—— 不计触发也不计未触发")
                }
            }
        }
        let fired = neg_results.iter().any(|r| matches!(r, NegResult::Fired(_)));
        let valid = neg_results
            .iter()
            .any(|r| !matches!(r, NegResult::Invalid(..)));
        if !valid {
            println!("  ⚠️ **负控全部无效**(都是传输故障):本次没有任何关于判据的证据。");
        } else if !fired {
            println!("  ⚠️ **负控未触发**:有效负控里没有一个让判据响。这不等于判据可用,");
            println!("     只说明本次没能证伪 —— 不要据此给 go。");
        }
    }
    println!(
        "请求用量 = {}/{MAX_REQUESTS}(3 正 + 2 负,无余量)",
        budget.used
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 模板模式:回放抓包实物,只替换身份占位符
// ═══════════════════════════════════════════════════════════════════════════
//
// ## 为什么要这个模式
//
// 两轮探针都是「出字正常但会话不注册」(无顶层 `.3`、无提交记录、无 `1.14` 用量帧)。
// 而离线 diff 已把 frame0 做到与实物字段级全等、ENV 的「我方多出」归零。加法猜测
// (再补哪个字段)已经不收敛 —— 用户的 fresh2 实验更是直接排除了整类假设:
// **官方帧 + 官方完整 51558B ENV + 全新 uuid,照样裸态**。
//
// 所以策略反转成从能跑通的形态做减法。第一步是把「实物回放」这条基线搬进探针的
// reqwest 通道(fresh2 当时走 curl),排除传输侧差异:
// - 模板过了 → 门控在我方构造里,逐块做减法定位最小必需集;
// - 模板也裸态 → 门控在帧外(传输层/头部时序),那时比对出站字节与头。
//
// ## 同长替换是这个模式成立的前提
//
// 三个占位符都是 36 字符 ASCII uuid,**等长替换**,于是 protobuf 的所有长度前缀与
// 外层 message 长度都不变,不需要重新编码嵌套结构。任何变长替换都要重算各层长度,
// 风险高一个量级。
//
// ⚠️ 实测确认过:首轮报文里**没有任何时间戳**(`.26` ts 属于续轮描述符 `1.1`,
// 而 turn1 的 `1.1` 是空块;ENV 详情块的 `.26` 是字符串 `"enabled"` 的 hooks 开关)。
// 所以替换清单里没有 ts —— 照"改 ts"去动那个字段反而会制造一处新差异。

const CAP_DIR: &str = "/tmp/cursor-capture";
const CAP_SESSION: &str = "1787457692061-0";
/// 实物里的旧标识符(自检时用来验零残留)。
const OLD_CONV: &str = "3d9e9788-015e-4ef4-a9d3-d13ed1023146";
const OLD_MSG: &str = "0cfdbbdd-3d28-45d6-be56-092b85ec42e6";
const OLD_REQ: &str = "64cb1414-b792-4703-8b87-50e1a1695a8a";

/// 替换后的模板帧。`frames` 已是可直接上线的 Connect 帧。
struct Template {
    conv: String,
    turn: String,
    msg: String,
    /// 帧0 与 ENV 的**未压缩 payload**(自检要在这上面 grep)。
    frame0_payload: Vec<u8>,
    env_payload: Vec<u8>,
    /// 全部 13 帧,已分帧(帧0 未压、ENV 重新 gzip、控制帧原样)。
    frames: Vec<Vec<u8>>,
}

/// 读一个抓包文件,返回 `(flag, 未压缩 payload, 原始整帧字节)`。
fn read_cap(name: &str) -> std::io::Result<(u8, Vec<u8>, Vec<u8>)> {
    let raw = std::fs::read(format!("{CAP_DIR}/{CAP_SESSION}.{name}.bin"))?;
    let flag = raw[0];
    let len = u32::from_be_bytes([raw[1], raw[2], raw[3], raw[4]]) as usize;
    let body = &raw[5..5 + len];
    let payload = wire::frame_payload(flag, body)
        .map_err(|e| std::io::Error::other(format!("解帧失败: {e}")))?;
    Ok((flag, payload, raw))
}

fn load_template() -> std::io::Result<Template> {
    let conv = uuid::Uuid::new_v4().to_string();
    let turn = uuid::Uuid::new_v4().to_string();
    let msg = uuid::Uuid::new_v4().to_string();

    // 等长替换(36 字符 → 36 字符),长度前缀不变。
    let subst = |b: &[u8]| -> Vec<u8> {
        let s = b.to_vec();
        let mut out = s;
        for (old, new) in [
            (OLD_CONV, conv.as_str()),
            (OLD_MSG, msg.as_str()),
            (OLD_REQ, turn.as_str()),
        ] {
            assert_eq!(old.len(), new.len(), "替换必须等长");
            out = replace_bytes(&out, old.as_bytes(), new.as_bytes());
        }
        out
    };

    let (_, f0, _) = read_cap("req-000")?;
    let (_, env, _) = read_cap("req-001")?;
    let frame0_payload = subst(&f0);
    let env_payload = subst(&env);

    let mut frames = vec![
        // 帧0:实物 flag=0 未压缩,照实物。
        wire::frame(&frame0_payload),
        // ENV:实物 flag=1,重新 gzip(压缩后大小会变,Connect 帧头自带长度,无妨)。
        wire::frame_compressed(&env_payload)
            .map_err(|e| std::io::Error::other(format!("gzip 失败: {e}")))?,
    ];
    // 控制帧 req-002..012:不含任何 uuid,**原样整帧回放**。
    for i in 2..=12 {
        let (_, _, raw) = read_cap(&format!("req-{i:03}"))?;
        frames.push(raw);
    }

    Ok(Template {
        conv,
        turn,
        msg,
        frame0_payload,
        env_payload,
        frames,
    })
}

fn replace_bytes(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// 离线自检:**不发任何请求**。验替换正确、零残留,并把帧落盘供人工核对。
fn template_selfcheck(t: &Template) -> bool {
    let mut ok = true;
    println!("=== 模板离线自检(不发请求)===");
    println!("新会话 uuid = {}", t.conv);
    println!("新 request uuid = {}", t.turn);
    println!("新消息 uuid = {}", t.msg);
    println!();

    let count = |hay: &[u8], needle: &str| -> usize {
        let n = needle.as_bytes();
        if n.is_empty() || hay.len() < n.len() {
            return 0;
        }
        (0..=hay.len() - n.len())
            .filter(|&i| &hay[i..i + n.len()] == n)
            .count()
    };

    // ① 零残留:旧会话 / 旧消息 / 旧 request uuid,以及旧 ts 的两种形态。
    let old_ts_dec = "1787457693360";
    let old_ts_varint: [u8; 6] = [0xb0, 0xcd, 0x9f, 0xe6, 0x82, 0x34];
    for (label, payload) in [("frame0", &t.frame0_payload), ("ENV", &t.env_payload)] {
        for (what, old) in [
            ("旧会话", OLD_CONV),
            ("旧消息", OLD_MSG),
            ("旧request", OLD_REQ),
        ] {
            let n = count(payload, old);
            if n != 0 {
                println!("  ✘ {label} 残留 {what} uuid ×{n}");
                ok = false;
            } else {
                println!("  ✔ {label} 无 {what} uuid 残留");
            }
        }
        let n = count(payload, old_ts_dec);
        let nv = payload
            .windows(old_ts_varint.len())
            .filter(|w| *w == old_ts_varint)
            .count();
        if n != 0 || nv != 0 {
            println!("  ✘ {label} 残留旧 ts(十进制×{n} varint×{nv})");
            ok = false;
        } else {
            println!("  ✔ {label} 无旧 ts 残留(十进制与 varint 两种形态均 0)");
        }
    }

    // ② 新标识符出现次数要与实物一致:frame0 会话×2、消息×1、request×1;ENV 会话×1。
    let checks: [(&str, &[u8], &str, usize); 4] = [
        ("frame0 会话 uuid", &t.frame0_payload, t.conv.as_str(), 2),
        ("frame0 消息 uuid", &t.frame0_payload, t.msg.as_str(), 1),
        ("frame0 request uuid", &t.frame0_payload, t.turn.as_str(), 1),
        ("ENV 会话 uuid", &t.env_payload, t.conv.as_str(), 1),
    ];
    println!();
    for (label, payload, needle, want) in checks {
        let got = count(payload, needle);
        if got == want {
            println!("  ✔ {label} ×{got}(与实物一致)");
        } else {
            println!("  ✘ {label} ×{got},实物应为 ×{want}");
            ok = false;
        }
    }

    // ③ 等长替换 ⇒ payload 长度必须与实物完全相同(长度一变就说明动了结构)。
    println!();
    match (read_cap("req-000"), read_cap("req-001")) {
        (Ok((_, f0, _)), Ok((_, env, _))) => {
            for (label, ours, real) in [
                ("frame0", t.frame0_payload.len(), f0.len()),
                ("ENV", t.env_payload.len(), env.len()),
            ] {
                if ours == real {
                    println!("  ✔ {label} payload 长度不变({ours} B)");
                } else {
                    println!("  ✘ {label} payload 长度变了:{ours} vs 实物 {real}");
                    ok = false;
                }
            }
        }
        _ => {
            println!("  ✘ 读不到实物文件,无法校长度");
            ok = false;
        }
    }

    // ④ 帧数与落盘
    println!();
    if t.frames.len() == 13 {
        println!("  ✔ 帧数 13(帧0 + ENV + 11 控制帧,与实物写序一致)");
    } else {
        println!("  ✘ 帧数 {} ,实物 13", t.frames.len());
        ok = false;
    }
    let _ = std::fs::write("/tmp/tmpl-frame0.bin", &t.frame0_payload);
    let _ = std::fs::write("/tmp/tmpl-env.bin", &t.env_payload);
    println!("  落盘:/tmp/tmpl-frame0.bin  /tmp/tmpl-env.bin(供人工 diff)");

    println!();
    println!(
        "自检结论:{}",
        if ok {
            "通过 —— 可以跑模板请求"
        } else {
            "**不通过,别跑**"
        }
    );
    ok
}
