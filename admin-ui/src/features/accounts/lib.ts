import type {
  AccountDisplayStatus,
  AccountGroupMembership,
  AccountRow,
  AccountRuntimeEntry,
  AccountRuntimeInstance,
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
  if (row.disabled) return { kind: 'disabled' }
  if (!runtime || !runtime.online) return { kind: 'offline' }

  const status = runtime.status
  if (!status.disabled) return { kind: 'ok' }

  switch (status.reason) {
    case 'rate_limited':
      return { kind: 'rate_limited', secs: status.cooldown_remaining_secs }
    case 'empty_response':
      return { kind: 'empty_response', secs: status.cooldown_remaining_secs }
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

/** 积分展示格式化:整数千分位。允许负值(超额账号的 remaining=已超出多少)。 */
export function formatCredits(n: number): string {
  return Math.round(n).toLocaleString()
}

/** 剩余是否吃紧(超额/为 0/< 上限 10%）—— 用于标红提示该换号。 */
export function isQuotaLow(remaining: number, limit: number): boolean {
  if (remaining < 0) return true // 超额一定标红(即使上限未知)
  if (limit <= 0) return false // 上限未知且未超额:不误报
  return remaining <= 0 || remaining / limit < 0.1
}

/** 配额展示口径:'windows' = 滚动窗口利用率(5h/7d,Claude OAuth/ccmax);
 *  'credits' = 积分剩余/上限(Kiro);'none' = 无配额概念(Cursor 订阅制,后端不采集)。 */
export type QuotaKind = 'credits' | 'windows' | 'none'

/** provider → 配额口径。dario(claude-dario)无积分概念,只有 5h/7d 利用率窗口;
 *  cursor 是订阅制、后端不采集配额数字,配额列恒为 —,用 'none' 免得表头误称「积分」。 */
export function quotaKindForProvider(provider: string): QuotaKind {
  if (provider === 'claude-dario') return 'windows'
  if (provider === 'cursor') return 'none'
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
