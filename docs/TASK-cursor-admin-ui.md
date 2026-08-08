# 任务：给 caio 后台加上 Cursor 账号的管理界面

> **只改前端 `admin-ui/`，不要动 `crates/` 里的任何 Rust 代码。**
> 后端能力已经全部就绪并且通过了真实上游验证，缺的只是后台界面。

## 0. 你要读的和不用读的

| 要读 | 不用读 |
|---|---|
| 本文档 | `crates/gw-cursor/PROTOCOL-agent-run.md`（748 行协议逆向，与前端无关） |
| `admin-ui/src/features/accounts/` 全目录 | `crates/` 下任何 Rust 文件 |
| `admin-ui/src/pages/AccountsPage.tsx` | |

写完之后会有人审你的代码，重点看两件事：**有没有沿用本仓既有范式**，以及**有没有编造不存在的接口**。

---

## 1. 背景：现在是什么状态

caio 是一个多账号反代网关，后端有四个 provider 家族：

| 家族名（`provider` 字段） | 是什么 | 后台支持 |
|---|---|---|
| `kiro` | Kiro 企业号 | ✅ 完整 |
| `claude-dario` | Claude OAuth（用户惯称 **ccmax**） | ✅ 完整 |
| `claude-subprocess` | 本地子进程 | 部分 |
| **`cursor`** | **Cursor 订阅** | ❌ **一处都没有** |

`cursor` 家族的后端已经写完：能对话、多轮、tool_use、thinking、图像、PDF，按账号独立出口，token 自动刷新。但 `grep -rn "cursor" admin-ui/src/` 目前**零命中**（除了 `cursor-pointer` 这种 CSS 类名）。

后果就是：Cursor 账号只能靠手写 SQL 或 curl 建，建完在后台列表里显示成一个没有标签的裸 provider 名，也没法筛选。

**你的任务：把这条链补上——能建、能看、能编辑、能筛。**

---

## 2. 先明确边界：哪些事不要做

这几条最容易好心做错，逐条说明理由。

### 2.1 ❌ 不要碰 `crates/` 下的 Rust 代码

后端接口已经够用（见 §3）。如果你觉得缺某个后端能力，**先在报告里写出来**，不要自己动手加。

### 2.2 ❌ 不要为「有状态会话」做任何 UI

后端有个实验性开关 `CURSOR_STATEFUL`（环境变量）。它**没有做通，开了会让请求挂起**，因此默认关闭。

它不是用户配置项，不该出现在界面上。看到相关代码或注释请忽略。

### 2.3 ❌ 不要给 cursor 账号做「档位/订阅等级」筛选

账号页现在有个「订阅档」筛选器，那是 **kiro 专属**的（读 `extra.subscription_title`）。Cursor 没有这个概念。

现有代码里这个筛选器已经用 `row.provider === 'kiro'` 守住了，**保持原样**，别扩展到 cursor。

### 2.4 ❌ 不要新建「Cursor 专属页面」

Cursor 账号和其它账号一样住在 `AccountsPage`。用户已经明确表达过这个偏好——之前有人给「自动补货」单独做了一个页面，被要求整合回主后台。

### 2.5 ⚠️ 后端**没有**暴露账号字段定义的接口

Rust 侧有个 `account_schema()` 声明了每个 provider 需要哪些字段，但**它没有任何 HTTP 端点**（我已经确认：`grep -rn "account_schema" crates/gw-app/src/` 零命中）。

所以**表单字段必须按 §4 的表格手写**，不要去找 `/admin/api/providers` 或 `/admin/api/schema` 之类的接口——**那些不存在**。

---

## 3. 后端接口（已存在，直接用）

前缀统一是 `/admin/api`。前端已有封装，**用 `features/accounts/hooks.ts` 里的 hook，不要自己写 fetch**。

### 3.1 建号：`POST /accounts` — 已有 `useCreateAccount()`

通用建号接口，能建任意 provider。请求体类型 `CreateAccountPayload` 已定义在 `features/accounts/types.ts`：

```ts
{
  account_id: string        // 必填
  provider?: string         // 传 'cursor'；不传默认 'kiro'
  group?: string            // 归属分组
  max_concurrency?: number
  priority?: number         // 越小越优先，缺省 100
  egress?: string           // ''/'direct'=直连, 'auto'=自动均衡, 数字=egress_pool 索引
  extra?: Record<string, unknown>   // 凭据等放这里，见 §4
}
```

**出口代理由后端自动分配**（没显式给 `extra.proxy` 且设置里配了 `egress_pool` 时，后端会挑最少使用的那个）。你不需要在前端实现分配逻辑，只要把 `egress` 选项传对。

### 3.2 其它已有接口

| 用途 | Hook | 说明 |
|---|---|---|
| 列表 | `useAccounts()` | 返回 `AccountRow[]`，`extra` 里的凭据已被后端脱敏成 `***xxxx` |
| 运行态 | `useAccountsRuntime()` | 在线/冷却等 |
| 编辑 | `useUpdateAccount()` | ⚠️ `extra` **传了就是整体替换** |
| 删除 | `useDeleteAccount()` | |
| 刷新 token | `useRefreshAccount()` | |

---

## 4. Cursor 账号的字段（照这张表写表单）

放进 `extra`。**这张表是权威**，来自 `crates/gw-cursor/src/lib.rs` 的 `CURSOR_ACCOUNT_SCHEMA`。

| 字段 | 标签 | 必填 | 类型 | 从哪来 / 说明 |
|---|---|---|---|---|
| `access_token` | Access Token | ✅ | **密码框** | Cursor session JWT。用户从 Cursor 的 `state.vscdb` 取 `cursorAuth/accessToken` |
| `refresh_token` | Refresh Token | ⬜ | **密码框** | 取 `cursorAuth/refreshToken`。**留空的后果要在 UI 上提示**：无法自动续期，token 过期后该号会被判失效下线 |
| `machine_id` | Machine ID | ⬜ | 文本 | 真 IDE 的 `telemetry.machineId`（64 位十六进制，取自 `storage.json`）。留空则后端按 token 派生 |
| `mac_machine_id` | Mac Machine ID | ⬜ | 文本 | 真 IDE 的 `telemetry.macMachineId`（64 位十六进制）。留空则后端派生 |
| `config_version` | Config Version | ⬜ | 文本 | **建议留空**——留空时后端每会话自动向上游取新鲜值 |
| `timezone` | 时区 | ⬜ | 文本 | 如 `Asia/Shanghai`。**应与该号出口 IP 的地理位置一致**，否则是账号关联特征。留空按 `Asia/Shanghai` |
| `proxy` | 出口代理 | ⬜ | 文本 | 一般留空走自动分配。填了则该号所有上游请求走它 |

字段顺序建议就按上表。`access_token` / `refresh_token` 两个必须用 `type="password"`（本仓既有做法，参考 `OAuthAccountDialog`）。

### 4.1 用户体验上的一个要点

`access_token` 和 `refresh_token` 在 Cursor 侧**本来就是同一个 JWT**（后端刷新时新的 access_token 同时兼任新的 refresh_token）。用户从 `state.vscdb` 里读出来会发现两个值可能一样，**这是正常的，不要在 UI 上报错或去重**。

---

## 5. 具体要改的文件

我已经把所有硬编码 provider 名的位置找全了。下面每条都给了确切的 `文件:行号`。

### 5.1 `features/accounts/lib.ts:237` — 加 tab 标签

```ts
export function providerTabLabel(provider: string): string {
  switch (provider) {
    case 'kiro':
      return 'Kiro'
    case 'claude-dario':
      return 'ccmax'
    // ← 在这里加 cursor
    case '':
      return '—'
    default:
      return provider
  }
}
```

标签建议用 `Cursor`。

**注意这个函数目前没有覆盖所有展示位置。** provider 筛选器（`AccountsFilterBar.tsx:58`）是
这样渲染选项的：

```ts
...providers.map((p) => ({ value: p.provider, label: `${p.provider} (${p.count})` }))
```

它用的是**裸 provider 名**，没走 `providerTabLabel`。也就是说筛选下拉里会出现
`claude-dario (7)` 而不是 `ccmax (7)`。

**顺手把它改成走 `providerTabLabel`** —— 这样标签只需在一个地方维护，
cursor 也就自动跟着对了。这是本任务里唯一一处「顺带修既有小问题」，值得做。

### 5.2 `features/accounts/lib.ts:159` — 确认配额口径

```ts
export function quotaKindForProvider(provider: string): QuotaKind {
  return provider === 'claude-dario' ? 'windows' : 'credits'
}
```

**判断题**：Cursor 有没有「积分余额」概念？

答案是**没有**——Cursor 是订阅制，后端也不采集它的配额数字。所以现在这行会把 cursor 判成 `'credits'`，但那一列对 cursor 恒为空。

**这条你自己决定怎么处理**，两种都可接受：
- 保持原样（配额列显示空）——改动最小；
- 或者加一个语义更准的分支。

不管选哪个，**在报告里说明你的选择和理由**。

### 5.3 `pages/AccountsPage.tsx:94` — 排序权重

```ts
const rank = (p: string) => (p === 'kiro' ? 0 : p === 'claude-dario' ? 1 : 2)
```

给 cursor 一个位置。建议排在 ccmax 之后（`kiro` 0 → `claude-dario` 1 → `cursor` 2 → 其它 3）。

### 5.4 建号入口 — 新建对话框

`AccountsPage.tsx:239-246` 现在有两个按钮：

```tsx
<Button variant="outline" onClick={() => setImportOpen(true)}>   {/* Kiro 导出 JSON 批量导入 */}
<Button variant="outline" onClick={() => setOauthOpen(true)}>    {/* ccmax OAuth 授权 */}
```

**两个都与家族绑定，cursor 用不上**：
- `ImportAccountsDialog` 吃 Kiro 专有的导出 JSON 格式；
- `OAuthAccountDialog` 走 `claude-dario` 的 OAuth 流程（第 109 行硬编码 `provider: 'claude-dario'`）。

所以新建一个 `features/accounts/components/CursorAccountDialog.tsx`，在 `AccountsPage` 加第三个按钮。

**范式请照抄 `OAuthAccountDialog.tsx`**，特别是：
- `Modal` / `Button` / `Select` 都从 `@/components/ui/` 取；
- 表单控件是**原生 input + 那个复制粘贴的 `inputClass` 常量**（第 17-18 行）——本仓 6 个文件都这么写，**没有** Switch 和 Toast 组件，不要引入新的 UI 库；
- 账号 ID 校验用 `ACCOUNT_ID_PATTERN`（从 `../types` 导入）；
- 分组下拉用 `useGroups()`，出口下拉用 `useSettings().data?.egress_pool`；
- 错误提示用 `extractErrorMessage` / `getErrorStatus`（从 `@/lib/api`）。

对话框应包含：账号 ID、分组、并发数、优先级、出口选择，加上 §4 的那些 `extra` 字段。

比 OAuth 那个**简单得多**——没有两步授权，填完直接 `POST /accounts`。

### 5.5 `lib/i18n.tsx` — zh 和 en 都要加

结构是：

```
第 12 行   const zh = { ... }                       ← 先在这里加 key
第 491 行  export type I18nKey = keyof typeof zh    ← 类型从 zh 推导
第 494 行  const en: Record<I18nKey, string> = {}   ← 这里漏一个 key 就 tsc 失败
```

⚠️ **`en` 是 `Record<I18nKey, string>`，漏 key 会直接编译报错。** 两边必须同步加。

### 5.6 `features/accounts/lib.test.ts` — 补测试

已有测试文件。你改了 `providerTabLabel`（和可能的 `quotaKindForProvider`）就补上对应用例。

---

## 6. 验收标准

```bash
cd admin-ui
bun install
bunx tsc --noEmit      # 必须零错误（i18n 漏 key 会在这里暴露）
bun run test           # 必须全绿
bun run build          # 必须成功
```

本机没有 node 18+/bun 的话，用 Docker：

```bash
docker run --rm -v "$PWD/admin-ui":/app -w /app oven/bun:1 bash -c "bun install && bunx tsc --noEmit && bun run test"
```

功能上自查这几条：

- [ ] 账号列表里 cursor 账号的 provider 列显示为 `Cursor` 而不是裸的 `cursor`
- [ ] provider 筛选器里的选项显示为 `Cursor (n)`（选项本身是按数据自动生成的，不用加代码；但标签要走 `providerTabLabel`）
- [ ] 新对话框能成功建出一个 cursor 账号（`provider: 'cursor'`，凭据在 `extra`）
- [ ] `EditAccountDialog` 能编辑 cursor 账号（**它完全不按家族分支，所以理论上无需改动** —— 但要实际点开验一遍，别只看代码就下结论）
- [ ] **375px 窄屏无横向滚动条**（本仓断点只用 `md` = 768px）
- [ ] 深色模式正常（用既有语义色 token，别写死颜色值）

---

## 7. 提交要求

- 分支：`feat/cursor-admin-ui`，**不要直接推 main**
- 提交信息用中文，说清改了什么、为什么
- ⚠️ **不要提交任何真实凭据**。仓库 `github.com/htesd/claude-all-in-one` 是**公开的**。测试数据用占位符
- 报告里请写明：
  1. §5.2 那道判断题你怎么选的、为什么
  2. 有没有发现文档与代码实际情况不符的地方（**有就直说，文档可能有错**）
  3. 有没有你认为该做但因为 §2 的限制没做的事

---

## 8. 一句话总结

**后端全通了，就差后台界面。**改 3 处硬编码 provider 名 + 新建 1 个建号对话框 + 补 i18n 两份词条 + 补测试。不要碰 Rust，不要为实验性开关做 UI，不要编造后端接口。
