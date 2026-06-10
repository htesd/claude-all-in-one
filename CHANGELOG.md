# Changelog

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
