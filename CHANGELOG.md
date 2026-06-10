# Changelog

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
