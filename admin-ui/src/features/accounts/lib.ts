import type {
  AccountDisplayStatus,
  AccountGroupMembership,
  AccountModelEntry,
  AccountRow,
  AccountRuntimeEntry,
  AccountRuntimeInstance,
  OnDemandQuota,
} from './types'

/** 把按 worker 实例组织的运行态摊平成 account_id 索引；同号出现在多实例时偏向在线实例。 */
export function mergeRuntimeByAccount(
  instances: AccountRuntimeInstance[] | undefined,
): Map<string, AccountRuntimeEntry> {
  const map = new Map<string, AccountRuntimeEntry>()
  for (const instance of instances ?? []) {
    for (const status of instance.accounts_status ?? []) {
      const existing = map.get(status.account_id)
      if (!existing || (instance.online && !existing.online)) {
        map.set(status.account_id, { status, online: instance.online })
      }
    }
  }
  return map
}

/**
 * 配置行 + 运行态 merge 出展示状态：
 * - 配置 disabled        => 已停用
 * - runtime 缺失/实例离线 => 离线/未服务
 * - runtime 启用         => 正常
 * - 其余按 reason 细分（冷却类带剩余秒数）
 */
export function deriveAccountStatus(
  row: AccountRow,
  runtime: AccountRuntimeEntry | undefined,
): AccountDisplayStatus {
  // 自动退役会落库 disabled=1:先判 row.disabled 会把 runtime 的 suspended_retired
  // 永远遮成普通「已停用」(红标签变死代码,退役号混进人工停用堆)。runtime 在线且
  // 报退役时优先展示退役。
  if (row.disabled) {
    if (runtime?.online && runtime.status.reason === 'suspended_retired') {
      return { kind: 'retired' }
    }
    return { kind: 'disabled' }
  }
  if (!runtime || !runtime.online) return { kind: 'offline' }

  const status = runtime.status
  if (!status.disabled) {
    // 复活观察期:冷却刚过、单飞探测中 —— 与"正常"区分开,免得排查时
    // 把单飞限流误判成并发闸坏了。
    if (status.probation) return { kind: 'probation' }
    return { kind: 'ok' }
  }

  switch (status.reason) {
    case 'rate_limited':
      return { kind: 'rate_limited', secs: status.cooldown_remaining_secs }
    case 'empty_response':
      return { kind: 'empty_response', secs: status.cooldown_remaining_secs }
    case 'temporarily_suspended':
      return { kind: 'suspended', secs: status.cooldown_remaining_secs }
    case 'suspended_retired':
      return { kind: 'retired' }
    case 'quota_exhausted':
      return { kind: 'quota_exhausted' }
    case 'invalid_refresh_token':
      return { kind: 'invalid_refresh_token' }
    case 'too_many_failures':
      return { kind: 'too_many_failures' }
    case 'config':
    default:
      // 'config' 或后端新增的未知原因：归并为「已停用」灰
      return { kind: 'disabled' }
  }
}

/**
 * 凭据轮换时构造整体替换的 extra：
 * 非敏感字段原样保留；脱敏字段（`***` 开头）也**原样回传**——后端把 `***` 前缀
 * 视为「保留 DB 原值」哨兵，这样带多个敏感字段（如 client_secret）的账号在只换
 * refresh_token 时不会丢其余凭据；最后写入用户新输入的 refresh_token。
 */
export function buildRotatedExtra(
  original: Record<string, unknown>,
  refreshToken: string,
): Record<string, unknown> {
  const next: Record<string, unknown> = { ...original }
  next.refresh_token = refreshToken
  return next
}

/** 取脱敏后的 refresh_token（尾号核对用）；没有则返回 null。 */
export function getMaskedRefreshToken(extra: Record<string, unknown>): string | null {
  const value = extra['refresh_token']
  return typeof value === 'string' && value !== '' ? value : null
}

/** 并发上限输入校验：>= 1 的整数；非法返回 null。 */
export function parseConcurrency(input: string): number | null {
  if (input.trim() === '') return null
  const value = Number(input)
  return Number.isInteger(value) && value >= 1 ? value : null
}

/**
 * 调度优先级输入校验：任意整数（数值越小越优先，允许 0 / 负数）；非法（空 / 非整数）返回 null。
 * 用 `Number()` + `Number.isInteger()` 而非 parseInt 字符串往返比较，避免误拒
 * `007` / `+5` / `1e3` 等 `<input type="number">` 语法合法的整数写法。
 */
export function parsePriority(input: string): number | null {
  if (input.trim() === '') return null
  const value = Number(input)
  return Number.isInteger(value) ? value : null
}

/**
 * 调度优先级两档:高=0,低=100。前端只暴露高/低两档;后端 priority 仍是 i64
 * (分层 LRU 支持任意层),故两档映射到固定数值即可,零数据迁移(线上恰为 0 / 100)。
 */
export type PriorityTier = 'high' | 'low'
export const HIGH_PRIORITY = 0
export const LOW_PRIORITY = 100

/** 两档 → 后端数值。 */
export function tierToPriority(tier: PriorityTier): number {
  return tier === 'high' ? HIGH_PRIORITY : LOW_PRIORITY
}

/** 后端数值 → 两档:< 100 视为「高」(兼容历史 0 及任意 < 100 的值),其余「低」。 */
export function priorityToTier(priority: number): PriorityTier {
  return priority < LOW_PRIORITY ? 'high' : 'low'
}

/**
 * 编辑账号时的成员边差集:草稿 vs 后端当前值。
 *
 * - `upserts` —— 新增的组,以及**只改了组内优先级**的组(后端 upsert 语义相同,同一个调用)
 * - `removals` —— 取消勾选的组名
 *
 * 两者都为空 = 没动过成员边,调用方**一个请求都不该发**。
 */
export interface MembershipDiff {
  upserts: AccountGroupMembership[]
  removals: string[]
}

export function diffMemberships(
  original: AccountGroupMembership[],
  draft: AccountGroupMembership[],
): MembershipDiff {
  const before = new Map(original.map((m) => [m.name, m.priority]))
  const upserts = draft.filter((m) => before.get(m.name) !== m.priority)
  const draftNames = new Set(draft.map((m) => m.name))
  const removals = original.filter((m) => !draftNames.has(m.name)).map((m) => m.name)
  return { upserts, removals }
}

/** 积分/美元展示:接近整数用整数千分位,否则保留两位小数(Cursor 官方额度是美元)。 */
export function formatCredits(n: number): string {
  if (Number.isFinite(n) && Math.abs(n - Math.round(n)) < 1e-9) {
    return Math.round(n).toLocaleString()
  }
  return n.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })
}

/** 模型不可用标记剩余时间的短格式:< 1h 显示分钟(下限 1m),否则显示小时(整点不带小数)。 */
export function formatMarkTtl(secs: number): string {
  if (secs < 3600) return `${Math.max(1, Math.round(secs / 60))}m`
  const h = secs / 3600
  return `${Number.isInteger(h) ? h : h.toFixed(1)}h`
}

/** 模型可用三态(「查看模型」弹窗的展示口径)。 */
export type ModelAvailability = 'available' | 'marked' | 'unsupported'

/**
 * 模型可用三态推导:档位不支持 > 被标记 > 可用。
 * 与后端 `available` 字段(supported && 无标记)同源,前端重算是为了把「不可用」
 * 细分出两种成因 —— 被标记(临时,附剩余重探时间)与档位不支持(静态,换档才变)。
 * `mark_remaining_secs` 为 0(即将到期)仍算标记中,与后端 `is_none()` 判定对齐。
 */
export function deriveModelAvailability(m: AccountModelEntry): ModelAvailability {
  if (!m.supported) return 'unsupported'
  if (m.mark_remaining_secs != null) return 'marked'
  return 'available'
}

/** 剩余是否吃紧(超额/为 0/< 上限 10%）—— 用于标红提示该换号。 */
export function isQuotaLow(remaining: number, limit: number): boolean {
  if (remaining < 0) return true // 超额一定标红(即使上限未知)
  if (limit <= 0) return false // 上限未知且未超额:不误报
  return remaining <= 0 || remaining / limit < 0.1
}

/** 美元金额短格式($ + 千分位;整数不带小数)。超额额度列专用。 */
export function formatUsd(n: number): string {
  return `$${formatCredits(n)}`
}

/**
 * 超额（on-demand）展示三态：
 * - `off`：未开启（面板显示「关」）
 * - `unlimited`：已开启但不限额
 * - `on`：已开启且有上限
 *
 * null/缺省（该 provider 无超额概念，或还没查到）由调用方按「—」处理。
 */
export function onDemandState(od: OnDemandQuota): 'off' | 'unlimited' | 'on' {
  if (!od.enabled) return 'off'
  if (od.unlimited || od.limit == null || od.limit <= 0) return 'unlimited'
  return 'on'
}

/**
 * 超额是否吃紧（已用 ≥ 上限 80%）—— 标黄提醒即将触顶（触顶后上游会开始拒绝请求）。
 * 不限额/未开启永不吃紧。
 */
export function isOnDemandHigh(od: OnDemandQuota): boolean {
  if (onDemandState(od) !== 'on' || od.limit == null || od.limit <= 0) return false
  return od.used / od.limit >= 0.8
}

/**
 * 模型白名单判定,**镜像后端 `gw-core::account::model_allowlist_allows` 的语义**:
 * - `null` / 缺失 = 不限(放行一切);
 * - 空数组 = 全禁(fail-closed,写侧正常不会存出这个形态);
 * - 条目小写精确匹配,`前缀*` 通配仅限末尾。
 * 用途:把账号现有白名单换算成「查看模型」弹窗的勾选基线。改这里必须同步后端匹配器。
 */
export function modelAllowlistAllows(
  allowlist: string[] | null | undefined,
  modelId: string,
): boolean {
  if (allowlist == null) return true
  const id = modelId.toLowerCase()
  return allowlist.some((raw) => {
    const entry = raw.toLowerCase()
    if (entry.endsWith('*')) return id.startsWith(entry.slice(0, -1))
    return id === entry
  })
}

/** 配额展示口径:'windows' = 滚动窗口利用率(5h/7d,Claude OAuth/ccmax);
 *  'credits' = 剩余/上限(Kiro=积分;Cursor=官方账期美元)。 */
export type QuotaKind = 'credits' | 'windows' | 'none'

/** provider → 配额口径。dario(claude-dario)无积分概念,只有 5h/7d 利用率窗口;
 *  cursor 走官方 GetCurrentPeriodUsage,与 Kiro 一样用剩余/上限列(单位美元,见 label)。 */
export function quotaKindForProvider(provider: string): QuotaKind {
  if (provider === 'claude-dario') return 'windows'
  // cursor 曾标 'none'(后端未采集);现已接 DashboardService,与积分列同形展示美元。
  if (provider === 'cursor') return 'credits'
  return 'credits'
}

/**
 * 展示状态 → 三分桶：ok / abnormal / disabled。
 * 用于 AccountsFilterBar 的状态筛选。
 */
export function accountStatusBucket(status: AccountDisplayStatus): 'ok' | 'abnormal' | 'disabled' {
  switch (status.kind) {
    case 'ok':
      return 'ok'
    case 'disabled':
      return 'disabled'
    default:
      // offline / rate_limited / empty_response / quota_exhausted /
      // invalid_refresh_token / too_many_failures
      return 'abnormal'
  }
}

/**
 * 账号订阅档推导：
 * 1. 优先用 runtime.quota.label（如 "KIRO PRO"）。
 * 2. 回落 row.extra.subscription_title（未脱敏字符串）。
 * 3. 大写后按关键字归档；无法识别时返回 null（未知）。
 * 仅对 kiro provider 有意义；其他 provider 直接传 undefined runtime 即可。
 */
export function deriveTier(
  row: AccountRow,
  runtime?: AccountRuntimeEntry,
): 'PRO' | 'POWER' | 'FREE' | 'OTHER' | null {
  const raw =
    (runtime?.status?.quota?.label ?? null) ||
    (typeof row.extra?.subscription_title === 'string' ? row.extra.subscription_title : null)
  if (!raw) return null
  const upper = raw.toUpperCase()
  if (upper.includes('POWER')) return 'POWER'
  if (upper.includes('PRO')) return 'PRO'
  if (upper.includes('FREE')) return 'FREE'
  return 'OTHER'
}

/**
 * 账号列表排序。
 *
 * 后端固定按 `group_name ASC, account_id ASC` 返回(见 gw-store `list_accounts`),
 * 那个顺序对「哪个号是刚上的」毫无帮助 —— 而 Kiro 的短命号(ksk_ API Key 寿命只有
 * 二三十分钟、且成批死亡)恰恰要按上号时间来管。故在前端补一层排序。
 *
 * 排序是**纯函数**且不改入参:调用方是 useMemo,原地 sort 会污染 react-query 缓存里的
 * 数组、让相同引用的后续 memo 读到被搅乱的顺序。
 */
export type AccountSortKey = 'created_desc' | 'created_asc' | 'name'

export function sortAccounts(rows: AccountRow[], key: AccountSortKey): AccountRow[] {
  const next = [...rows]
  switch (key) {
    case 'created_desc':
      // 同一批导入的号 created_at 完全相同(秒级),再按 id 兜底保证顺序稳定,
      // 否则 15s 轮询刷新时同批号会互相跳位。
      return next.sort(
        (a, b) => b.created_at - a.created_at || a.account_id.localeCompare(b.account_id),
      )
    case 'created_asc':
      return next.sort(
        (a, b) => a.created_at - b.created_at || a.account_id.localeCompare(b.account_id),
      )
    case 'name':
      // 后端原序:先分组再账号名。
      return next.sort(
        (a, b) => a.group_name.localeCompare(b.group_name) || a.account_id.localeCompare(b.account_id),
      )
  }
}

/** provider → 账号页 tab 短标签(用户惯用语:claude-dario 即 "ccmax")。未知 provider 原样。 */
export function providerTabLabel(provider: string): string {
  switch (provider) {
    case 'kiro':
      return 'Kiro'
    case 'claude-dario':
      return 'ccmax'
    case 'cursor':
      return 'Cursor'
    case '':
      return '—'
    default:
      return provider
  }
}

/**
 * 账号页 provider 筛选项：**后端支持的 provider 常驻**，计数为 0 也照样列出。
 *
 * 原先只按库里现有账号 `GROUP BY provider`，于是新接的通道在上第一个号之前
 * 筛选条里根本不出现 —— 而"能不能上号"恰恰是要验证的第一件事（2026-08-09
 * cursor 上线即遇到：只看到 Kiro / ccmax，让人以为部署缺了东西）。
 *
 * `KNOWN_PROVIDERS` 按 Kiro→ccmax→Cursor 排；库里出现的未知 provider（后端新增
 * 但前端还没跟上）追加在后面并按名字排序，不会因为不在名单里就被吞掉。
 */
export const KNOWN_PROVIDERS = ['kiro', 'claude-dario', 'cursor'] as const

export interface ProviderTab {
  provider: string
  count: number
}

export function buildProviderTabs(rows: AccountRow[] | undefined): ProviderTab[] {
  const counts = new Map<string, number>()
  for (const p of KNOWN_PROVIDERS) counts.set(p, 0)
  for (const row of rows ?? []) {
    counts.set(row.provider, (counts.get(row.provider) ?? 0) + 1)
  }
  const rank = (p: string) => {
    const i = (KNOWN_PROVIDERS as readonly string[]).indexOf(p)
    return i === -1 ? KNOWN_PROVIDERS.length : i
  }
  return [...counts.entries()]
    .sort((a, b) => rank(a[0]) - rank(b[0]) || a[0].localeCompare(b[0]))
    .map(([provider, count]) => ({ provider, count }))
}

/**
 * Cursor 建号表单的原始输入（均为未 trim 的字符串；access_token 必填，其余可空）。
 * 字段名与后端 CURSOR_ACCOUNT_SCHEMA 一一对应。
 */
export interface CursorExtraInput {
  access_token: string
  refresh_token?: string
  machine_id?: string
  mac_machine_id?: string
  config_version?: string
  timezone?: string
  proxy?: string
}

/**
 * Cursor 建号的 extra 组装：access_token 必填（调用方已校验非空）；
 * 可选字段 trim 后为空一律**省略**——空串会顶掉后端「留空 = 派生/默认」的语义
 * （machine_id/mac_machine_id 按 token 派生、config_version 每会话取新、
 *  timezone 按 Asia/Shanghai、proxy 走 worker 默认出口）。
 */
export function buildCursorExtra(input: CursorExtraInput): Record<string, unknown> {
  const extra: Record<string, unknown> = { access_token: input.access_token.trim() }
  for (const key of [
    'refresh_token',
    'machine_id',
    'mac_machine_id',
    'config_version',
    'timezone',
    'proxy',
  ] as const) {
    const value = input[key]?.trim()
    if (value) extra[key] = value
  }
  return extra
}

/**
 * Cursor 官方登录的一次轮询结果（喂给决策函数的输入）。
 * - pending：后端回 200，还没授权
 * - done：后端回 201，账号已落库
 * - error：axios 异常；status 为 HTTP 状态码，undefined = 传输层错误（无响应）
 */
export type CursorLoginPollOutcome =
  | { kind: 'pending' }
  | { kind: 'done' }
  | { kind: 'error'; status?: number }

/** 轮询决策：继续 / 成功收尾 / 终态失败 / 超时。 */
export type CursorLoginPollAction = 'continue' | 'success' | 'fail' | 'timeout'

/**
 * Cursor 登录轮询状态机（纯函数，便于单测）：
 * - done 永远算成功——凭据已落库，即使刚好越过截止线也不能按超时误报；
 * - 越过 expires_in_sec 窗口 → timeout（会话已被后端清扫，再问也是 400）；
 * - 502（后端到 Cursor 的瞬时网络错误）与传输层错误（无 status）→ 继续，会话还在；
 * - 其余 4xx/5xx → 终态失败（4xx 会话已清；未约定的其他状态码保守按终态处理）。
 */
export function decideCursorLoginPoll(
  outcome: CursorLoginPollOutcome,
  timedOut: boolean,
): CursorLoginPollAction {
  if (outcome.kind === 'done') return 'success'
  if (timedOut) return 'timeout'
  if (outcome.kind === 'pending') return 'continue'
  if (outcome.status === undefined || outcome.status === 502) return 'continue'
  return 'fail'
}
