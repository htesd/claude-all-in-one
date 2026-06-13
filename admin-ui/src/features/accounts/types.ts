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
  /**
   * 出口代理 URL。
   * - 非空字符串 = 设置该账号代理
   * - 空字符串 `""` = 清除（走全局默认）
   * - 不传 = 不动
   * 注意：不要传 null，后端以 null 表示"不修改"。
   */
  proxy_url?: string
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

/** 账号配额(积分)只读快照,来自 worker 的 getUsageLimits;尚未查到时为 null。 */
export interface AccountQuota {
  /** 已用额度(Credits)。 */
  used: number
  /** 额度上限。 */
  limit: number
  /** 剩余 = limit - used(可为负:超额账号显示已超出多少)。 */
  remaining: number
  /** 已用百分比(可超 100 = 已进 overage)。 */
  percent_used: number
  /** 订阅/单位标签(如 KIRO PRO),可空。 */
  label?: string | null
}

export interface AccountRuntimeStatus {
  account_id: string
  priority: number
  disabled: boolean
  reason: AccountUnavailableReason
  cooldown_remaining_secs: number
  failure_count: number
  available_permits: number
  max_concurrency: number
  /** 配额(积分);null = 后台查询中/未取到。 */
  quota?: AccountQuota | null
}

/** POST /accounts/import 请求体。 */
export interface ImportAccountsPayload {
  group_name?: string
  /** KiroManager 导出内容(原文字符串或已解析对象均可)。 */
  json: string
  /** 批量出口代理：非空时应用到本次导入的所有账号。 */
  batch_proxy?: string
}

/** POST /accounts/import 响应。 */
export interface ImportAccountsResult {
  created: number
  merged: number
  skipped: number
  items: Array<{
    account_id: string
    action: 'created' | 'merged' | 'skipped'
    has_machine_id?: boolean
    machine_id_conflict?: boolean
    reason?: string
  }>
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
