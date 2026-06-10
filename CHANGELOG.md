# Changelog

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
