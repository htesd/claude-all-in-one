import type {
  AccountDisplayStatus,
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
 *  'credits' = 积分剩余/上限(Kiro)。决定账号页该 tab 的配额列语义。 */
export type QuotaKind = 'credits' | 'windows'

/** provider → 配额口径。dario(claude-dario)无积分概念,只有 5h/7d 利用率窗口。 */
export function quotaKindForProvider(provider: string): QuotaKind {
  return provider === 'claude-dario' ? 'windows' : 'credits'
}

/** provider → 账号页 tab 短标签(用户惯用语:claude-dario 即 "ccmax")。未知 provider 原样。 */
export function providerTabLabel(provider: string): string {
  switch (provider) {
    case 'kiro':
      return 'Kiro'
    case 'claude-dario':
      return 'ccmax'
    case '':
      return '—'
    default:
      return provider
  }
}
