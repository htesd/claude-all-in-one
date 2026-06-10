/**
 * GET /admin/api/accounts 单行。
 * extra 中含 token/secret/password 的字段已由后端脱敏为 `***xxxx`（尾 4 位）。
 */
export interface AccountRow {
  account_id: string
  /** 所属分组名；'' = 未分组。 */
  group_name: string
  provider: string
  max_concurrency: number
  disabled: boolean
  extra: Record<string, unknown>
  /** 创建时间，Unix 秒。 */
  created_at: number
}

/** POST /accounts 请求体（注意：这里的分组字段叫 `group`，PATCH 才是 `group_name`）。 */
export interface CreateAccountPayload {
  account_id: string
  group?: string
  provider?: string
  max_concurrency?: number
  extra?: Record<string, unknown>
}

/** PATCH /accounts/{id}：extra 传了就是整体替换（凭据轮换），不传不动。 */
export interface UpdateAccountPayload {
  /** '' = 移出分组。 */
  group_name?: string
  max_concurrency?: number
  disabled?: boolean
  extra?: Record<string, unknown>
}

/** worker 侧账号不可用原因枚举（'' = 无）。 */
export type AccountUnavailableReason =
  | ''
  | 'rate_limited'
  | 'empty_response'
  | 'quota_exhausted'
  | 'invalid_refresh_token'
  | 'too_many_failures'
  | 'config'

export interface AccountRuntimeStatus {
  account_id: string
  priority: number
  disabled: boolean
  reason: AccountUnavailableReason
  cooldown_remaining_secs: number
  failure_count: number
  available_permits: number
  max_concurrency: number
}

/** GET /accounts/runtime 单条：一个 worker 实例（按 group 服务）。 */
export interface AccountRuntimeInstance {
  instance: string
  group: string
  online: boolean
  accounts_status?: AccountRuntimeStatus[]
}

/** 按 account_id 合并后的运行态条目。 */
export interface AccountRuntimeEntry {
  status: AccountRuntimeStatus
  online: boolean
}

/** 表格展示用的状态归一（配置 + 运行态 merge 的结果）。 */
export type AccountDisplayStatus =
  | { kind: 'disabled' }
  | { kind: 'offline' }
  | { kind: 'ok' }
  | { kind: 'rate_limited'; secs: number }
  | { kind: 'empty_response'; secs: number }
  | { kind: 'quota_exhausted' }
  | { kind: 'invalid_refresh_token' }
  | { kind: 'too_many_failures' }

/**
 * 账号 ID 规则（与后端一致）：1–64 个 URL-safe 字符 [A-Za-z0-9._~-]。
 * 提交前先在客户端校验，避免一次必然 400 的请求。
 */
export const ACCOUNT_ID_PATTERN = /^[A-Za-z0-9._~-]{1,64}$/
