/**
 * 该账号在某个分组里的成员边：**决定谁能用它 + 在那个组里排第几**。
 * 一个号可以同时是多个组的成员，每组一个独立优先级。
 */
export interface AccountGroupMembership {
  name: string
  /** 组内优先级，数值越小越优先（前端只暴露高=0 / 低=100 两档）。 */
  priority: number
}

/**
 * GET /admin/api/accounts 单行。
 * extra 中含 token/secret/password 的字段已由后端脱敏为 `***xxxx`（尾 4 位）。
 */
export interface AccountRow {
  account_id: string
  /**
   * **归属**分组名；'' = 未分组。只表示哪个 worker 独占管它的运行态，
   * **不是权限** —— 谁能用它由 `groups`（成员边）决定。
   */
  group_name: string
  provider: string
  max_concurrency: number
  /**
   * `extra.priority`，缺省 100。**调度不读它** —— 重构后组内排序在成员边上（见 `groups`），
   * 这里只是导入时的默认种子。别拿它当排序依据展示。
   */
  priority: number
  disabled: boolean
  extra: Record<string, unknown>
  /**
   * 该号的全部成员边，后端按组名升序返回（顺序稳定，可直接与编辑草稿做差集）。
   * 旧缓存响应可能缺失 —— 缺失视为“未知”，空数组才是“不在任何组”。
   */
  groups?: AccountGroupMembership[]
  /** 创建时间，Unix 秒。 */
  created_at: number
  /** 累计成功请求数（后端新增；旧缓存响应可能缺失，缺省视为 0）。 */
  success_count?: number
  /** 累计失败请求数（后端新增；旧缓存响应可能缺失，缺省视为 0）。 */
  failure_count?: number
}

/** POST /accounts 请求体（注意：这里的分组字段叫 `group`，PATCH 才是 `group_name`）。 */
export interface CreateAccountPayload {
  account_id: string
  group?: string
  provider?: string
  max_concurrency?: number
  /** 调度优先级：数值越小越优先，缺省 100（不传则由后端按 100 处理）。 */
  priority?: number
  extra?: Record<string, unknown>
  /** 出口网关选择：''/'direct'=直连；'auto'=自动均衡；数字字符串=egress_pool 索引。 */
  egress?: string
  /**
   * claude-dario 专用：粘贴 CC .credentials.json 全文。
   * 后端解析 claudeAiOauth 块并并入 extra（access_token / refresh_token / expires_at）。
   */
  credentials_json?: string
}

/** PATCH /accounts/{id}：extra 传了就是整体替换（凭据轮换），不传不动。 */
export interface UpdateAccountPayload {
  /** '' = 移出分组。 */
  group_name?: string
  max_concurrency?: number
  disabled?: boolean
  /** 调度优先级：数值越小越优先，缺省 100。不传=不动；走后端定点合并，绝不碰凭据。 */
  priority?: number
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

/** 一个用量窗口(如 dario 的 5h / 7d 滚动窗口),利用率%。 */
export interface QuotaWindow {
  /** 窗口标签,如 "5h" / "7d"。 */
  label: string
  /** 已用利用率(0–100+,可超 100 = 已进 overage)。 */
  percent_used: number
  /** 该窗口重置的 unix 秒(可空)。 */
  reset_at?: number | null
}

/** 账号配额只读快照;尚未查到时为 null。Kiro=积分(used/limit);dario=利用率窗口(windows)。 */
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
  /** 多窗口利用率(dario 的 5h/7d);空/缺省 = 基于积分的 provider(Kiro),走 remaining/limit 显示。 */
  windows?: QuotaWindow[]
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
  /** 批量出口代理：非空时应用到本次导入的所有账号(API 直连用;UI 走 egress)。 */
  batch_proxy?: string
  /** 出口网关选择：''/'direct'=直连；'auto'=自动均衡；数字字符串=egress_pool 索引。 */
  egress?: string
}

/** POST /accounts/import-apikeys 请求体：粘贴的官方 API Key（ksk_）列表。 */
export interface ImportApiKeysPayload {
  group_name?: string
  /** 粘贴文本，每行一个 ksk_...（空白/逗号分隔均可）。 */
  keys: string
  /** 出口网关选择：''/'direct'=直连；'auto'=自动均衡；数字字符串=egress_pool 索引。 */
  egress?: string
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
