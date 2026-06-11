# Changelog

## [settings-panel-and-egress-proxy] - 2026-06-12

### Features
- **系统设置面板(前端首个设置页)+ DB 持久 + 30s 热生效**:`settings` 单行表(JSON overlay)叠在不可变的 `system.yaml` 基线上;`GET/PUT /admin/api/settings`(GET=有效全量,PUT=部分 patch,`null`/空=删该 overlay 字段回 YAML 默认)。worker 30s 轮询用 `from_effective` 广播全量回 provider(`apply_hot_settings`)+ `scheduler.update_tuning` + cache_sim,**无需重启**。前端 `SettingsPage` 四组卡片(代理/缓存/调度/图像),只提交改动字段。
- **全局默认代理 + 每账号出口代理(全程同出口)**:新 `gw-kiro/src/resolver.rs::EgressResolver` 按账号解析出口 client,优先级 **账号 `extra.proxy` → 全局 `default_proxy` → worker 绑定源 IP**。KiroProvider 的 chat/refresh/quota/profileArn **四处**统一走 `resolver.client_for(account)`——同一账号刷新与发包同 IP(防封铁律)。代理 client 用 `reqwest::Proxy`(不绑 local_address);无代理才用 base(绑源 IP)。
- **每账号代理写入通道**:创建账号 `extra.proxy`;导入 `batch_proxy`(整批);编辑 `PATCH proxy_url`(定点 `merge_account_extra`,空串=清除,**绝不碰凭据**,规避 PATCH extra 整块替换坑)。前端三对话框对应加字段。
- **调度 `Tuning` 热更**:`RwLock<Tuning>` + `update_tuning`;`KiroProvider` 的 cache_billing/image_cfg 改 `RwLock` 承接热调。

### Design Rationale
- 不改 `ProviderFactory` 签名:`default_proxy` 经 `provider_cfg` JSON 注入 + `Provider::apply_hot_settings`(默认 no-op)——改动局限在 Kiro。
- 对抗审查(Codex×3)修复:①**代理写入边界 fail-closed**(`validate_proxy_url`:非法/含掩码占位的代理在 create/import/update/settings-PUT 一律 400,杜绝"配了代理却静默回退裸 IP");②**代理密码脱敏**(`redact_proxy_url`:GET 响应把 `user:pass@` 的密码段掩成 `***`,真值仍存库供 resolver 用);③**`deny_unknown_fields`**(设置 PUT 拼错 key 直接 400,不落库死 overlay);④Provider trait 出口契约文档更新(不再"出口由进程固定")。

### Notes & Caveats
- **默认代理热更的请求内一致性**:账号**无专属代理**、恰好在一次请求的 refresh 与 chat 之间被 admin 改了全局 `default_proxy` 时,这一次可能两步走不同出口(极窄窗口、自愈)。**用每账号专属代理(完全稳定)做严格按号隔离**,不要在有流量时热改全局默认代理。彻底冻结"请求级出口身份"需重构 provider/worker 出口流程,代价大、暂不做。
- `default_proxy` 仅 DB 管理(不读 `system.yaml` 基线);设置 PUT 的读-改-写非单事务(单运营者并发可忽略);前端数字字段暂无"逐字段重置回 YAML 默认"(可手动填默认值)——均为已知后续项。
- 进程拓扑(端口/每 worker 源 IP)仍在 instances.yaml,改动需重启(面板已注明)。

## [scheduler-hardening-and-image-compression] - 2026-06-11

### Features
- **模型能力过滤(opus 防误杀)**:`Provider::account_supports_model`(Kiro 实现 = `subscription_title` 含 FREE 拒 opus,未知放行)+ `AccountScheduler::acquire_where(谓词)` + 新错误 `NoModelSupport`(HTTP 400)。实测:纯 FREE 池打 opus 在选号阶段即拒,**零上游调用**——此前会 403 → 误判 TokenInvalid → 永久禁用健康号。
- **订阅档位数据闭环**:getUsageLimits 的 `subscriptionTitle` 回填 `extra.subscription_title`(内存锁内单字段合并 + DB 持久化);worker 启动/账号 sync 后对缺该字段的账号**预热**配额查询(只读,quota_sem=3 节流)。只导 rt 的老号也能收敛。
- **调度/冷却参数 config 化**(`system.yaml scheduler` 段),默认对齐 kiro.rs 生产:429 冷却 60s→**300s**、empty 冷却 20s→**60s**、empty 窗口 120s→**60s**、亲和 TTL 1800s;`max_failures=5` 保留(本项目 5xx 计数、kiro.rs 不计,语义不同)。
- **救号 reset 贯通**:`scheduler.reset_account`(清运行时禁用/冷却/计数,**配置禁用不动**)→ worker `POST /accounts/{id}/reset`(仅 loopback 挂载)→ admin 扇出端点 → 前端 HeartPulse 按钮(运行时禁用/有失败计数才显示)。
- **图像压缩移植**(🔵 kiro.rs/xkiro):四档阈值缩放 + **解码前 OOM 护栏**(1 亿像素/64MB 上限拦解压炸弹)+ 信号量背压 + spawn_blocking;失败一律回退原图。`system.yaml image` 段可配,接在 KiroProvider::chat 转换前。
- **`POST /v1/messages/count_tokens`**:router 本地估算(对齐 kiro.rs 默认路径),补 NewAPI/客户端探测兼容。
- `max_concurrency` 默认 1→**2**(serde/admin create/导入/SQLite schema/前端表单五处对齐 kiro.rs);DB 存量行不受影响,可 PATCH。

### Design Rationale
- 对抗审查(Codex×3)发现并已修复:①刷新回写「替换→置脏」非原子,30s sync 可在窗口内用 DB 旧值洗掉新 rolling token(新增 `update_account_dirty` 单锁原子化);②`flush_dirty_extras` 用旧快照整块落库+无条件清脏,会回滚并发刷新刚写库的新 token(改为逐账号持 refresh_lock、锁内重读+重查脏位);③刷新基底改用 scheduler 真值而非调用方旧快照(防用已作废 rolling token 刷新、防抹掉 merge 进来的字段);④全灭自愈带模型过滤(opus 请求不复活无关 FREE 失败号);⑤acquire 尝试预算与 max_failures 解耦(max_failures=1 时自愈后仍有机会重选);⑥reset 端点仅 loopback 挂载(非 loopback 误配不暴露无鉴权写操作)。
- 冷却状态仍纯内存(QuotaExhausted 重启复活,撞一次 402 重禁),与 kiro.rs 的差异已知、影响小,暂不持久化。

### Notes & Caveats
- 调度参数改动需重启 worker;热调控制面与 cache 参数热调一起留作后续(跨进程 plumbing)。

## [profile-arn-discovery] - 2026-06-11

### Features
- **动态 profileArn 发现(`ListAvailableProfiles`)** —— 🔵 对齐 static_flow,**kiro.rs 无此能力**(它依赖导入时凭据自带 profileArn)。企业/IdC 号的 chat 与 getUsageLimits 都强制要求 profileArn,凭据常不带;`gw-kiro/src/profiles.rs` 在缺失且无固定兜底(social/builderid)时,运行时 POST `q.{region}.amazonaws.com/ListAvailableProfiles`(跨候选区、翻页、runtime UA)发现并经 `ensure_profile_arn` 持久化进 extra。一次发现、后续短路。
- `Provider::discover_profile_arn`(默认 None)+ KiroProvider 实现 + worker 在 chat/配额前 `ensure_profile_arn`(发现失败不阻断,让上游自然 400 BadRequest,不惩罚账号)。

### Design Rationale
- 发现失败(403「not authorized」= 个人/Builder ID 层无此功能)时静默回退,由固定兜底或显式 profileArn 接管。复用 `persist_extra_field`(refresh_lock 互斥,与 subscription_title 同协议)。

### Notes & Caveats
- **实测验证(真号端到端,IdC Builder ID PRO 账号)**:
  - getUsageLimits → `KIRO PRO` 已用 9456/1000(945% 超额),subscription_title 回填成功;
  - opus 非流式 chat → `OK` + end_turn + usage(cache_read=683/input=6150);
  - opus 流式 chat → 完整 SSE 事件序列(message_start…message_stop);
  - count_tokens 真实内容 → 估算正常;模型过滤 opus 路由正确;BadRequest **不封号**(账号始终 enabled)。
- **发现**:Builder ID 个人号(clientName「Amazon Q Developer for command line」)的 `ListAvailableProfiles` 返回 403「not authorized」,须靠 `kiro_provider=builderid` 走固定 `BUILDER_ID_PROFILE_ARN` 兜底;原始 JSON 凭据(无 kiro_provider)目前需手填该字段。**待办**:对此类 403 自动回退 builder ARN,让裸凭据即插即用(本次保守未做,避免误判真企业号)。
- **运营提醒**:`PATCH /accounts/{id}` 的 `extra` 是**整块替换**(凭据轮换语义),漏字段会清空凭据——admin UI 编辑须回填全字段或改后端为字段级 merge(待办)。

## [thinking-xhigh-and-converter-hardening] - 2026-06-11

### Features

睡前 backlog 的安全增量(thinking 深度 + 缓存配置 + converter 400 兜底):

- **thinking 默认 effort `high`→`xhigh`**:Opus 全系默认 adaptive 思维链时,缺省 effort 之前是 `high`(实测仅产 ~43 字符桩推理),改为 `xhigh`(~3560 字符深推理),对齐 static_flow。`OutputConfig.effort` 改为 `Option<String>` + `effective_effort()` 缺省 xhigh(客户端带 output_config 但不带 effort 时也能正确回退,不再被 serde 默认强写 high)。
- **cache_sim 会话表可配**:`CacheConfig` 加 `sim_ttl_secs`(默认 300)/`max_sessions`(默认 4096),worker 启动时同步到全局 sim store(此前恒用编译期默认)。带 serde 默认,旧 system.yaml 仍可解析。
- **converter 400 兜底两项**(纯正确性,对齐 static_flow):空工具描述兜底为 `Client-provided tool '{name}'`(某些 Kiro 版本拒空描述 400);文档 `source.type="text"`(markdown/html/csv/txt)现 base64 编码后透传(此前静默丢弃)。
- **identity_override + 隐私策略注入**(`converter/history.rs`,逐字对齐 static_flow):每个请求 history[0] 始终注入,强制模型自认 Claude、不自曝 Kiro。⚠️ **实测发现不足**:用真号"你是谁"探针,模型注入后**仍答"我是 Kiro"**——Kiro 上游服务端身份压过客户端注入。单测证实注入确实落线缆;故此为 static_flow 平价(无害该留),**非身份检测银弹**。真实检测向量是结构化输出/desc/thinking 泄漏(见 docs 计划),需 38990 探针重放定位。

### Notes & Caveats

- 实测:gw-kiro 264 测试全绿(含 thinking effort/空描述/text 文档新单测);workspace 全绿。**未对真号发 chat**。
- **未做(故意推迟,见 docs/CONTEXT_LEGITIMACY_AND_TUNING_PLAN.md)**:① identity_override + 隐私策略注入(防身份检测封号)——**检测敏感,需 38990 探针重放验证后再上**,不盲发;② 更大的 converter 防 400 项(tool_use ID 清洗/文档去重限额/多模态工具 schema 兼容/stringified tool_result 解析);③ cache 三参数运行时热调端点(跨进程,需 router↔worker 内网跳);④ 前端补齐(请求日志/调度面板/强制刷新/批量操作)。

## [import-and-quota] - 2026-06-11

### Features

完整导入 KiroManager 账号 + 账号配额(积分)展示:

- **完整导入(防封核心)**:新增 `POST /admin/api/accounts/import` + 前端 `ImportAccountsDialog`,粘贴/上传 KiroManager 导出 JSON 一键导入。`gw-kiro/src/import.rs` 把导出字段映射到账号 extra——**关键是搬运真机 `machineId`**:此前只导 refreshToken,服务器据 rt 重派生一个不同 machineId(`sha256("KotlinNativeAPI/"+rt)`)→ 上游看到"激活设备 A、发包设备 B" = 双指纹 = 封号;完整导入消除这一根因。同时搬 clientId/secret/profileArn/region/kiro_provider。
- **智能合并**:已存在账号只补缺失身份字段;**token 字段(refresh_token/access_token/expires_at)仅创建时写,合并永不碰**(服务器拥有并轮换,导出里是旧值)。`machineId` 与已有不同时不覆盖、标 `machine_id_conflict` 提示。
- **账号配额展示**:`gw-kiro/src/usage_limits.rs` 移植 kiro.rs 的 `getUsageLimits`(只读),Provider 新增 `account_quota`。worker 侧 stale-while-revalidate 缓存(TTL 60s)+ 并发上限信号量,`/health` 带 `quota` 字段,前端账号表加"积分(剩余/上限)"列(吃紧标红)。

### Design Rationale

- **machineId 是防封关键,非端点**:KiroManager 导出 JSON 顶层带激活时的真机 `machineId`,完整导入原样搬入即可让发包指纹与激活一致。
- **token 合并即创建时写**:既兑现"不回退服务器已 roll token",又消除"导入读到旧值→并发刷新写新值→导入覆盖回旧值"的 TOCTOU 竞态(无需把合并塞进 DB 事务)。
- **配额只读 + 不阻塞 /health**:getUsageLimits 是只读查询(用户确认不招封号),后台刷新 + 缓存,/health 立即返回缓存值;信号量挡住"上百账号同时被查看"的 stampede。

### Notes & Caveats

- **配额刷新会 roll token**:后台刷新调用 ensure_credentialed,token 临期时会刷新(roll rt)。对已托管给反代的账号是预期行为;若同时还在用 KiroManager 管同一账号,两边 token 会发散。
- **machineId 必须合法**:导入只接受 64hex/UUID 形态的 machineId(非法形态丢弃,留空靠冻结按 rt 派生并提示),避免"谎报已设置但运行时仍派生"。
- **account_id 由 email 清洗派生**:可读但可能碰撞;智能合并用 user_id/email 稳定身份核对,不同真号撞同一 ID 时跳过(绝不合并两个真号)。
- getUsageLimits 走 `q.amazonaws.com`(同 kiro.rs)、发包走 `runtime.kiro.dev`,不同 host 但 machineId 一致(真实客户端本就跨 host)。
- 实测:workspace 测试全绿(gw-app 80 / gw-kiro 258,含导入映射/智能合并/碰撞/token保留/配额解析);admin-ui tsc+vite 通过;两个真号(POWER/PRO)经实时端点导入验证 machineId 落库后清理。**未对真号发任何 chat**。
- 对抗审查(codex Skeptic+Architect+Minimalist):修 3 个 high(account_id 碰撞合并/非法 machineId 谎报/token 覆盖竞态)+ 配额失败节流 + stampede 信号量 + 去 `backfilled` 泄漏字段名 + json 单一字符串形态;保留 Provider trait 默认方法(与现有 affinity_key 一致)。

## [kiro-wire-align] - 2026-06-10

### Features

gw-kiro 报文标准化 + machineId 防封 —— 逐字节对齐当前生产客户端 **static_flow**(commit 9051d71,已 `git fetch` 更新到最新):

- **主推理端点迁移**:`q.{region}.amazonaws.com` → `runtime.{region}.kiro.dev`(env `KIRO_RUNTIME_UPSTREAM_BASE_URL` / `KIRO_UPSTREAM_BASE_URL` 可覆盖)。kiro.rs 旧实现仍停在 `q.amazonaws.com` 且写死不可配,本项目对齐 static_flow 当前客户端。
- **新模块 `gw-kiro/src/headers.rs`**(报文单一事实源):主推理请求头逐字对齐——`accept: application/vnd.amazon.eventstream`、UA `os/darwin#24.6.0`/`nodejs#22.22.0`、**主 UA 去掉 `m/E`**(此前为残缺指纹)、条件头 `TokenType: EXTERNAL_IDP`(external_idp)/`redirect-for-internal`(internal provider)。chat.rs/token.rs/lib.rs 的 UA 全部收敛至此,消除版本漂移(此前 machine_identity 写死 `aws-sdk-js/1.0.0` 的陷阱)。
- **IdC 刷新 UA 对齐**:x-amz-user-agent 带版本 `KiroIDE-0.12.155`;user-agent 去掉 `api/sso-oidc`、补齐 os/node 版本。
- **machineId 冻结防封**(核心):`machineId = sha256("KotlinNativeAPI/"+refresh_token)`,而 refresh_token 是 **rolling** 的——不冻结则每次刷新 machineId 漂移 = 上游视为"同账号换设备" = 封号。`freeze_machine_id_if_absent` 在 `refresh_auth` **覆盖新 token 之前**用旧 token 派生值钉成显式 `machine_id` 并经 worker delta 持久化,设备指纹此后恒定。
- **账号 schema 扩字段**:暴露 `machine_id`(防封关键,可填真机指纹)、`auth_method`、`client_id`/`client_secret`(IdC 一等公民)、`kiro_api_key`、`kiro_version`,admin 表单可配置三种凭据(Social/IdC/API Key)。`FieldSpec` 加 `with_help` 提示。
- **脱敏补全**(安全):admin GET 脱敏此前只认 `token`/`secret`/`password`,漏掉 `kiro_api_key`(含 `key`)→ 明文泄漏。现加入 `key` 规则,所有 `*_key` 凭据字段一并脱敏(PATCH `***` 哨兵保留逻辑不受影响)。

### Design Rationale

- **为什么是 machineId 而非端点**:经 kiro.rs / static_flow / gw-kiro 三方代码交叉验证,KiroManager 导入(Social 号)易封、JSON 导入(常为 IdC)不易封的根因是 machineId 指纹漂移——Social 号注册绑真机 `vscode.env.machineId`,导出常丢失该值,派生哈希对不上;且 rolling token 让派生值持续漂移。端点 `q.amazonaws.com` 仍可用(static_flow 自身也用于 ListAvailableProfiles),属"不够像当前客户端"的次要指纹,非已证实封号主因。
- **冻结在 provider 的 refresh_auth**:此处持有刷新前的旧 token(派生材料),且返回的 extra 经既有 worker delta 持久化机制落库,无需新增跨层管道。
- **headers.rs 单一事实源 + golden 单测**:把"对齐 static_flow"显式化为逐字断言的单测(`streaming_ua_matches_static_flow_exactly` 等),static_flow 再更新时测试即对照点,防止悄悄漂移。

### 对抗审查加固(codex Skeptic + Architect,CONTESTED → 处置)

- **撤下未实现的 API Key 路径**(high):此前 schema 暴露 `kiro_api_key` 但调用链 OAuth-only(refresh_auth 强制 refresh_token、chat 只读 access_token),会"加载通过、首请求才报错"。现从 schema 移除,`validate_account` 明确要求 refresh_token 并提示 API Key 暂不支持。
- **profileArn 固定兜底**(high):端点迁 runtime.kiro.dev 后,缺 profileArn 可能被拒/命中错误 profile。port static_flow 的 `fixed_profile_arn`:按 `kiro_provider`(github/google→social 共享 ARN;builderid→builder ARN)兜底,显式值优先,企业号仍省略(动态 ListAvailableProfiles 未实现)。
- **IdC 刷新报文逐字对齐**(medium):补 `accept: */*`、头序对齐 static_flow `refresh_idc`,并抽到 `headers::apply_idc_refresh_headers` + golden 测试(此前 golden 不覆盖 IdC 刷新)。
- **machineId 误判修复**(medium):`is_api_key_credential` 改为必须有非空 `kiro_api_key`(仅 `auth_method=api_key` 标签不够),避免误配账号落随机指纹再被冻结固化。
- **provider 撞名修复**(medium):`redirect-for-internal` / profileArn 兜底改读专用键 `kiro_provider`(`extra["provider"]` 会被 serde flatten 吃到 `Account.provider` 顶层字段)。
- 安全:admin 脱敏补 `key` 规则,`kiro_api_key` 等 `*_key` 字段不再明文经 GET 泄漏(Architect 复核已确认修复)。

### Notes & Caveats

- 冻结的局限(如实声明):若账号导入时 refresh_token 已 roll 过(KiroManager 导出的是当前而非原始 token),冻结值仍 ≠ 真机指纹,只是阻止"继续漂移";彻底规避需在账号里填真机 `machine_id`(schema 已支持)。Social 号首次刷新时 info 日志提示。
- `kiro_version` 可 per-account 覆盖,但 OS/Node/SDK 版本写死:改它而不同步会造成现实不存在的指纹组合(schema help 已警示),非必要勿动。
- 端点统一 `runtime.{region}.kiro.dev`:暂未保留 static_flow 对 gov region 的 `q-fips.*` 特殊 host(当前无 gov 账号;需要时经 env 覆盖)。
- "对齐 static_flow" 靠 vendored 常量 + golden 单测(注明源 commit 9051d71),非自动同步:static_flow 升级 client/SDK 版本时需手动比对更新(已 `git fetch` 到最新)。endpoint-family 抽象(MCP/usage/profile 路径)暂缓(当前无 MCP 上游路径)。
- 实测:gw-kiro 测试(含 headers golden 10 + machineId 冻结/收紧 + IdC golden);workspace 387 全绿,零警告。报文未对真实上游发包验证(凭据/风控约束),靠 golden 单测对照 static_flow 源码保证字节一致。

## [rename] - 2026-06-10

### Features

- 项目更名:**kiro-gw → Claude All in One**。二进制 `claude-all-in-one`、admin UI 标题/登录页、文档、示例配置、前端包名全部跟随;localStorage 键前缀 `kiroGw*` → `caio*`(已登录会话需重输一次 admin token)。

### Notes & Caveats

- 内部 crate 名(gw-core/gw-kiro/gw-store/gw-app)是实现细节,未随名;上游 provider "Kiro" 的指称(gw-kiro、账号示例 kiro-01)保留——那是上游名,不是项目名。
- 启动命令换为 `./target/debug/claude-all-in-one --mode ...`;旧名二进制已从 target 删除,防误用旧产物。

## [admin-v1.1] - 2026-06-10

### Features

后端完善四件套 + 停机数据安全:

- **router 负载计数修正**:删掉只增不减的累计计数器,活跃负载改为从亲和表派生(钉在该 worker 上的未过期 session 数)。session 过期负载即回落,空闲 worker 能重新承接新会话;亲和指向已下线 worker(拓扑变更)时丢弃重选。
- **admin `/accounts/runtime` 并行聚合**:逐 worker 拉 `/health` 由串行改 `join_all` 并发,最坏耗时 ≈ 单个 2s 超时,不再随离线 worker 数累加。
- **优雅停机**:router/worker 响应 SIGTERM/Ctrl-C(`with_graceful_shutdown`)——停止接收新连接,在途请求(含流式 SSE)自然跑完;排空不设上限,硬截止由 supervisor 兜底(docker 默认 10s、systemd `TimeoutStopSec`)。
- **worker 停机前脏 extra 落盘**:`flush_dirty_extras` 由 30s sync 循环与停机排空共用——「刷新成功但 DB 回写失败」的 rolling refresh_token 在进程退出前有最后一次落盘机会,不再依赖下轮 30s 重试(进程退出即 drop)。
- **`--features embed-ui` 单二进制部署**:rust-embed 把 `admin-ui/dist` 嵌进二进制;SPA 客户端路由兜底回 index.html;vite 哈希资产 `immutable` 永久缓存、index.html `no-cache` 保发布即生效。feature 默认关闭(fresh clone 无 dist 仍可编译),关闭时维持原 ServeDir 磁盘读取。

对抗审查(Skeptic+Architect)加固:

- **router 故障转移**(Architect high):worker 进程挂掉但仍在配置里时,原实现会让钉住它的 session 502 长达 30 分钟。现 `send()` 连接失败 → 丢弃指向故障实例的亲和、在其余 worker 里重选重发一次(请求未送达,无重复送达风险),亲和重钉到备选。活体实测:打挂掉的 instance 0 → 自动转移 instance 1 拿到正常响应,第二个请求直达不再转移。
- **停机等待在途 usage 落库**(Skeptic medium):SSE 收尾的 usage/quota 落库是 Drop 里 detach 的 spawn 任务,graceful shutdown 只等响应体不等它们。新增 `PendingWrites` RAII 登记,排空后 `wait_idle`(5s 上限)等这批任务收尾,最后一批计费记录不再随 runtime 关闭静默丢失。
- **亲和全表清理节流**(Skeptic medium):O(n) retain 从每请求改为 ≥5s 一次(`cleanup_if_due`);命中路径补 O(1) 精确过期判断(不依赖清理兜底)。几十万 session 时 router 延迟不再随表大小线性放大;代价是负载统计里陈旧条目最多滞留 5s。

### Design Rationale

- **负载从亲和表派生而非独立计数**:单一事实源,过期清理(retain)天然让负载回落,无需配对的 increment/decrement(后者漏一边就永久漂移——正是被替换实现的病根)。代价是「负载」语义为活跃 session 数而非在途请求数,对会话粘性网关是合理代理指标。
- **embed-ui 用 feature 门控而非无条件嵌入**:rust-embed 编译期要求资产目录存在,`dist` 又被 gitignore;无条件嵌入会让 fresh clone 直接编译失败。release 部署构建用 `cargo build --release --features embed-ui`(需先 `bun run build`)。
- **停机落盘只兜「已脏」数据**:正常路径刷新成功即同步落库(admin-v1 已做),脏位只在落库失败时存在;停机 flush 是窄窗口兜底而非主路径。

### Notes & Caveats

- 实测链:负载回落/重选/故障转移/PendingWrites 有单测(注入 now / in-memory store);内嵌单二进制从无 dist 目录起服 curl 验证(index/哈希资产/SPA 兜底/缓存头/admin api);SIGTERM 实测日志+1s 内干净退出;故障转移双 worker 活体实测(挂 0 → 转移 1 → 亲和重钉)。
- 「活跃负载」语义是亲和 session 数,不是在途请求/流数(Architect medium,接受):一个 session 开多条长 SSE 只计 1。换真实在途计数留给下阶段(届时需拆亲和表双职责)。
- 负载均衡对「无 session_id」请求不计负载(无亲和记忆,一发即走)。
- worker 运行态(冷却/封禁)仍是内存态,重启即清——优雅停机不改变这一点(既有设计:持久化冷却反而会在重启后误恢复过期冷却;DB 只存配置)。
- embed-ui 与默认 ServeDir 两条路径的缓存头有漂移(ServeDir 无 Cache-Control;Architect low,接受):ServeDir 仅用于开发迭代,生产走 embed。

## [admin-v1] - 2026-06-10

### Features

admin 控制面完整落地(嵌入 router 进程,`/admin` SPA + `/admin/api/*`,单一 `admin.token` 鉴权,常量时间比较):

- **用量看板/用量页**:总览卡、按模型、按客户 apikey 三维度;时间窗(近 7/30 天/全部/自定义起止)+ 按 key 筛选;未归属流量单列桶。
- **API Keys 管理**:列表(掩码+复制)、新建(服务端 `sk-gw-<uuid4>` 生成或导入自定义 key,字符集 `[A-Za-z0-9._~-]{8,128}`)、备注、启停(逐请求查库即时生效)、删除(usage 历史保留)。
- **账号管理(yaml → SQLite)**:`accounts.yaml` 启动幂等导入(只播种,绝不覆盖已 roll 的 token),此后 DB 是配置事实源;admin CRUD;worker 30s 周期 sync——增删改免重启生效;**token 刷新成功先回写 DB**,rolling refresh_token 重启不丢;运行态(冷却剩余/禁用原因/并发占用)由调度器快照经 worker `/health` 暴露,admin `/accounts/runtime` 聚合;凭据响应一律脱敏保尾 4 位。
- **分组**:groups 表(色板/备注/账号数/key 数);账号与 key 都可归组;删组成员转未分组(事务),不级联删。
- **按客户限额(计费 v1)**:`api_keys.quota_tokens/used_tokens`,UsageSink 落库时锁内累加(口径 input+output,未归属不计);鉴权 SQL 内算 `over_quota`,超额回 429 `rate_limit_error`;admin 可设额/清除/重置已用,即时生效。

### Design Rationale

- **DB=配置事实源,worker 内存=运行态**:配置(账号/key/组/限额)进 SQLite 由 admin 管理;冷却/封禁/并发等运行态留在调度器内存经 HTTP 快照暴露——避免高频运行态写库,也避免重启后误恢复过期冷却。
- **sync 翻转语义**:配置 `disabled` 仅在**翻转**时触碰运行态(→false 视为 admin 显式复活)。同值周期 sync 绝不洗掉风控冷却/封禁,防止 30s 轮询反复"救活"被风控的账号。
- **统计读连接分离**:admin 全历史聚合走独立只读连接(WAL 并发),控制面再慢也压不到数据面的鉴权与计费落库。
- **限额单位用 token(input+output)**:v1 求简单可解释;加权成本(cache 折扣/模型价差)留到计费表达式阶段,届时只需替换累加口径。
- **busy_timeout 必须最先设**:router/worker 并发启动同一 WAL 库,否则抢锁直接 "database is locked" 即死。

### Notes & Caveats

- 明文 key 仍兼任身份/PK/usage 归属键:key 轮换、同名重建会合并历史归属。引入稳定 `key_id` 规划在加权计费阶段一并做。
- keys/accounts 列表无分页,客户量大(千级)后需要加。
- 限额检查在鉴权时读、settlement 时写:并发在途请求可少量超额(误差 ≤ 在途并发量),对人为限额场景可接受。
- admin 改账号生效有 ≤30s 的 sync 延迟(UI 已提示)。
- 部署:`admin-ui/dist` 由 router 运行时读取(改前端需 `bun run build`);单二进制内嵌(rust-embed)待做。
