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
  /**
   * 是否开启「排队等冷却」（`extra.queue_enabled`）。
   * 开了以后，该号在 429 冷却中时请求会**等它自愈**而不是立刻 503。
   * 逐号开关：企业号的上游并发跨租户共享，429 是跟别人抢、等一下就有；
   * 社交号的 429 常伴额度见底，等待只会把客户多挂几秒。
   * 旧缓存响应可能缺失 → 缺省视为 false（关）。
   */
  queue_enabled?: boolean
  /**
   * 模型白名单（`extra.model_allowlist`，后端顶层回显）。
   * 规范化后的字符串数组（小写，条目为 Run 侧模型名或「前缀*」）；
   * null / 缺失 = 不限。旧缓存响应可能缺失 → 视为不限。
   */
  model_allowlist?: string[] | null
  /**
   * 上游驱动形态（`extra.driver`，后端顶层回显）。
   * - `'cli'` = 子进程驱动官方 cursor-agent（usage 是上游真实值、含 cacheRead；
   *   工具经 MCP 桥回路）
   * - null / 缺失 = 默认的线协议形态（自己拼 protobuf 打 agent.v1.AgentService/Run）
   *
   * **只对 cursor 家族有意义**：别的 provider 读不到这个键。
   * 旧缓存响应可能缺失 → 视为线协议。
   */
  driver?: string | null
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
  /** 排队开关。不传=不动；走后端定点合并，绝不碰凭据。 */
  queue_enabled?: boolean
  /**
   * 模型白名单（逗号分隔串，UI 输入形态）。
   * - 非空 = 设置（后端校验：通配符只许末尾、非法字符 400；规范化成 JSON 数组落库）
   * - 空字符串 `""` = 清除（不限）
   * - 不传 = 不动
   * 走后端定点合并，绝不碰凭据。
   */
  model_allowlist?: string
  /**
   * 上游驱动形态（cursor 专用）。
   * - `'cli'` = 子进程驱动 cursor-agent
   * - 空字符串 `""` = 清除（回默认线协议）
   * - 不传 = 不动
   * 走后端定点合并，绝不碰凭据；认不出的值后端 400。
   */
  driver?: string
}

/** worker 侧账号不可用原因枚举（'' = 无）。 */
export type AccountUnavailableReason =
  | ''
  | 'rate_limited'
  | 'empty_response'
  | 'quota_exhausted'
  | 'invalid_refresh_token'
  | 'too_many_failures'
  | 'temporarily_suspended'
  | 'suspended_retired'
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

/** 账号配额只读快照;尚未查到时为 null。Kiro=积分(used/limit);Cursor=官方账期美元;dario=利用率窗口(windows)。 */
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
  /** 超额(on-demand)额度;null/缺省 = 该 provider 无此概念或未查到。 */
  on_demand?: OnDemandQuota | null
  /**
   * 账号档位('FREE' / 'PAID' / provider 档位名);null/缺省 = 判不出来。
   *
   * 为什么要单列一栏:被上游**降级成 FREE** 的号,`used`/`limit` 会一起变成 0
   * (免费号没有套餐内额度字段),面板上和"配额查询失败"长得一模一样。
   * 有这一栏才能一眼分清"降级"和"查不到"。
   */
  plan_tier?: string | null
}

/**
 * 超额(on-demand / usage-based)额度快照。金额单位**美元**。
 *
 * 与 `AccountQuota.used/limit`(套餐内额度)是两笔独立的账:套餐用尽后才吃超额。
 * 目前只有 Cursor 有(`DashboardService/GetHardLimit` + `spendLimitUsage`)。
 */
export interface OnDemandQuota {
  /** 是否已开启超额。 */
  enabled: boolean
  /** 超额上限(美元);null = 未开启 / 不限额 / 上游未给。 */
  limit?: number | null
  /** 本账期已用超额(美元)。0 是正常值(未产生超额消费),不是"未知"。 */
  used: number
  /** 是否不限额(上游用 i32 上限表示)。 */
  unlimited: boolean
}

/** 一条 (账号,模型) 不可用标记(INVALID_MODEL_ID)的运行时快照。 */
export interface ModelUnavailableMark {
  model: string
  /** 标记剩余存活秒数(到期自动重探)。 */
  remaining_secs: number
}

/** GET /accounts/{id}/models/local 单个模型条目(静态目录 + 档位支持判断 + 已学标记)。 */
export interface AccountModelEntry {
  id: string
  /** 目录显示名；上游未给时为 null。 */
  display_name: string | null
  /** 该账号订阅档位是否静态支持此模型。 */
  supported: boolean
  /** 已学 INVALID_MODEL_ID 标记的剩余秒数（查询时快照）；无标记为 null（0 = 即将到期,仍算标记中）。 */
  mark_remaining_secs: number | null
  /** 后端结论:supported 且无标记。三态细分用 deriveModelAvailability 重算。 */
  available: boolean
}

/**
 * GET /accounts/{id}/models/local 响应：账号可用模型清单。
 * **纯本地认知**(未观察到拒绝 ≠ 上游保证),全程零上游调用。
 */
export interface AccountModelsLocalResult {
  account_id: string
  models: AccountModelEntry[]
  /** 目录外的标记(上游新模型/请求变体名打不中目录行),单独透出。 */
  off_catalog_marks: ModelUnavailableMark[]
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
  /** 该号是否开了排队（worker 侧实时值；旧 worker 可能缺失 → 视为 false）。 */
  queue_enabled?: boolean
  /** 连续 suspend 退避档位；旧 worker 缺失 → undefined。 */
  suspend_streak?: number
  /** 是否处于复活观察期（单飞）；旧 worker 缺失 → undefined/false。 */
  probation?: boolean
  /** 当前生效的 (账号,模型) 不可用标记；旧 worker 缺失 → undefined（视为无标记）。 */
  model_unavailable?: ModelUnavailableMark[]
}

/**
 * 一个 worker 的排队实况。
 *
 * `capacity` 只统计**开了排队且当前可服务**的号的并发之和 —— 额度跑干/禁用的不计入。
 * 所以 `waiting/capacity` 是真实的拥挤度，不会因为库里躺着一堆跑干的号而虚高。
 */
export interface QueueStats {
  /** 此刻正在排队等冷却的请求数。 */
  waiting: number
  /** 队列容量（准入阈值）；`waiting` 触到它，新请求立刻 503 而不是排进来陪跑。 */
  capacity: number
  /** 开了排队开关的号数（不论当前是否可用）。 */
  enabled_accounts: number
  /**
   * **累计**进过排队的请求数（worker 启动以来）。
   * `waiting` 是瞬时值、几乎恒为 0（排队只在全组不可用时触发），看不出机制有没有在工作；
   * 累计值才能回答"这个开关到底救到人没有"。旧 worker 不返回 → 缺省视为 0。
   */
  queued_total?: number
  /**
   * **累计**被节流吸收的 429 次数。节流日志是 debug 级、线上看不到，这是它唯一的可观测面。
   */
  paced_total?: number
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
  /** 排队实况；旧 worker 不返回 → null/缺失，UI 需按“未知”降级而不是显示 0。 */
  queue?: QueueStats | null
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
  | { kind: 'probation' }
  | { kind: 'rate_limited'; secs: number }
  | { kind: 'empty_response'; secs: number }
  | { kind: 'suspended'; secs: number }
  | { kind: 'retired' }
  | { kind: 'quota_exhausted' }
  | { kind: 'invalid_refresh_token' }
  | { kind: 'too_many_failures' }

/**
 * 账号 ID 规则（与后端一致）：1–64 个 URL-safe 字符 [A-Za-z0-9._~-]。
 * 提交前先在客户端校验，避免一次必然 400 的请求。
 */
export const ACCOUNT_ID_PATTERN = /^[A-Za-z0-9._~-]{1,64}$/
