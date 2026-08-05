/** 自动补货：与 `/admin/api/restock/*` 对应的类型。 */

/** 运行时可调参数。**存在 control.db 里，不是 SystemSettings** —— 后者有回滚地板。 */
export interface RestockParams {
  enabled: boolean
  dry_run: boolean
  min_healthy: number
  max_per_purchase: number
  daily_cap_cny: number
  rate_cap: number
  min_balance_reserve_cny: number
  import_fail_breaker: number
  max_price_usd: number
  poll_interval_secs: number
  grave_ttl_hours: number
  peak_start: string
  peak_end: string
  utc_offset_minutes: number
  import_group: string
  member_groups: string[]
  egress: string
  new_account_concurrency: number
  new_account_queue_enabled: boolean
  forecast_hours: number
  idle_skip_ratio: number
  /** 可接受的单位成本上限（¥/积分）。买号策略的主旋钮，0 = 关闭。 */
  max_unit_cost_cny_per_credit: number
  account_throughput_credits_per_hour: number
  expected_lifetime_secs: number
  demand_window_secs: number
  liveness_window_secs: number
  new_account_grace_secs: number
  lead_time_secs: number
}

/** 参数上下限。后端 BOUNDS 表的投影，前端据此渲染表单并做前置校验。 */
export interface ParamBound {
  key: keyof RestockParams
  kind: 'bool' | 'int' | 'float' | 'hhmm'
  min: number
  max: number
  label: string
  hint: string
}

export interface RestockParamsResponse {
  configured: boolean
  spec: ParamBound[]
  values: RestockParams
}

/** 一个货架 = 同一批号、同一个价、同一个服务区。 */
export interface ShelfView {
  /** 供应商内部的货架标识（kiroapp 的 `us` / `eu`；drop 没有 = 空串）。 */
  shelf: string
  /** 人读的货架名，如 `kiroapp/eu`。 */
  label: string
  /** 该货架发出的号所属 Kiro 服务区。**与 `shelf` 是两个命名空间**。 */
  region: string
  stock: number
  /** 单价，**已归一到 ¥**（按限价汇率折算，是上界；两家同汇率所以比价准确）。 */
  unit_price_cny: number
  max_per_order: number
  /**
   * 该货架的**生效档位**（数值越小越优先，已含逐货架覆盖）。
   *
   * 显示它是必须的：`shelf_priority` 的键写错（`EU` vs `eu`）不会报错，只会静默
   * 回落本家档位。这个数字是唯一能让写错当场看得见的地方。
   */
  priority?: number
}

/** 一家货源在面板上的样子。 */
export interface SupplierView {
  id: string
  kind: string
  enabled: boolean
  /** 名册里启用了，但密钥缺失/客户端建不出来 → false。 */
  configured: boolean
  /** 空 = 此刻可以从这家买；非空是人读的原因（熔断 / 超本家日上限）。 */
  blocked: string
  /** 本轮询价失败的原因；null = 问通了。 */
  error: string | null
  balance_cny: number | null
  /** 对方原生单位的余额（如 `680 积分`）。**仅供对账用眼睛核**，别拿去算。 */
  balance_native: string
  spent_today_cny: number
  /** 档位，数值越小越优先。同档才比价。 */
  priority?: number
  /** 逐货架档位覆盖，键是货架标识（`us` / `eu`）。 */
  shelf_priority?: Record<string, number>
  /** 本家日上限；0 = 不限，由全局日上限兜底。 */
  daily_cap_cny: number
  shelves: ShelfView[]
}

export interface RestockSnapshot {
  /** **实证还在服务**的号数（caio 报正常 + 近期真的成功过）。水位比的就是它。 */
  healthy?: number
  /** caio 报「正常」但实证已死。>0 说明面板上那几个正常号是尸体。 */
  zombie?: number
  cooling?: number
  dead?: number
  total?: number
  any_online?: boolean
  /** 逐家视图。多供应商之后，「额度」不再是一个数。 */
  suppliers?: SupplierView[]
  /** 以下四个都是**最便宜那个货架**的数，不是某一家的。 */
  stock?: number
  price_usd?: number
  price_cny?: number
  best_shelf?: string | null
  /**
   * **下一单会买哪个货架**，由后端的 `choose_shelf` 本人回答（含全部花钱闸门）。
   * null = 这一刻买不成，理由在 `next_pick_why`。
   *
   * 前端**不要**自己再算一遍：排序能复刻，余额/单价上限/日上限/unit_cost_veto
   * 这些闸门复刻不了，猜错的时候正是面板最不该说谎的时候。
   */
  next_pick?: string | null
  /** `next_pick` 为 null 时的逐货架被否理由。 */
  next_pick_why?: string | null
  balance_cny?: number
  /** 所有货源余额之和。「我还剩多少钱」在多家之后只有这个数说得准。 */
  balance_total_cny?: number
  stock_at?: number
  at?: number
  drop_ok?: boolean
  drop_error?: string | null
  /** 当前需求速率（积分/时，全池口径）。 */
  demand_rate?: number
  /** 按当前需求预估的单位成本（¥/积分）；产出为 0 时后端给 null。 */
  expected_unit_cost?: number | null
  /** 实测寿命中位数（秒）。**仅供人工校准** expected_lifetime_secs，不自动生效。 */
  measured_lifetime_secs?: number
  measured_lifetime_samples?: number
}

export interface RestockDecisionRow {
  ts: number
  action: 'skip' | 'buy' | 'import' | 'reclaim' | 'error'
  reason: string
  healthy: number | null
  stock: number | null
  price_usd: number | null
  balance_cny: number | null
  detail: string
}

export interface RestockState {
  configured: boolean
  enabled: boolean
  dry_run: boolean
  /** 非空即熔断，内容是原因。 */
  breaker: string
  in_peak: boolean
  peak_window: string
  min_healthy: number
  daily_cap_cny: number
  spent_today: number
  bought_today: number
  /** 买到了 key 却没上号的订单数 —— 钱花了号没进系统，要人工处理。 */
  orphan_orders: number
  /** 哪个进程持有补货租约。生产有多个 router，这个数能确认互斥生效了。 */
  lease_holder: string | null
  snapshot: RestockSnapshot
  decisions: RestockDecisionRow[]
}

export interface CreditPoint {
  ts: number
  hour: number
  weekday: number
  ksk: number
  credits: number
  ksk_calls: number
  calls: number
  partial: boolean
}

export interface ForecastPoint {
  ts: number
  weekday: number
  hour: number
  /** 预测的**总**消耗（不是 ksk_ 那一份）—— 需求由客户流量决定，与谁在承接无关。 */
  credits: number
  /** 这个数拿什么算的：周画像 / 日画像 / 近期均值 / 数据不足。 */
  basis: string
  samples: number
}

export interface Coverage {
  hours_collected: number
  days_collected: number
  week_cells_ready: number
  hour_cells_ready: number
  mature: boolean
}

export interface RestockCredits {
  series: CreditPoint[]
  forecast: ForecastPoint[]
  forecast_hours: number
  forecast_demand: number
  coverage: Coverage
  models: { model: string; credits: number }[]
  buys: { ts: number; spent_cny: number }[]
  peak_start: string
  peak_end: string
  utc_offset_minutes: number
}

export interface RestockAccount {
  account_id: string
  created_at: number
  disabled: boolean
  max_concurrency: number
  /** 终身调用数（源自永不裁剪的 usage_records，不是 1.5 小时的日志窗口）。 */
  calls: number
  success: number
  credits: number
  groups: string
  cost_cny: number | null
  self_bought: boolean
  /** ¥/次成功调用。 */
  unit_cost: number | null
  /** ¥/积分。积分才是号的真实刻度（opus 一次约 1.1 分、haiku 0.03 分，差 40 倍）。 */
  unit_cost_per_credit: number | null
  /** 实际服务时长（秒）＝ 首末次用量之差。实测这批号一律 0.7–0.9 小时且与烧速无关。 */
  served_secs: number | null
  first_used_at: number | null
  last_used_at: number | null
  /** 运行态原因，空串 = 正常。区分「被风控封」与「key 被吊销」用。 */
  reason: string
}

export interface RestockAccountsResponse {
  count: number
  items: RestockAccount[]
}

/**
 * 名册里的一家（可写配置）。
 *
 * **响应里永远没有 `api_key`**，只有 `has_key` —— 掩码回显曾经引发过
 * 「把 `***` 当成真值存回去」的事故，所以连掩码都不给。
 */
export interface SupplierConfig {
  id: string
  kind: 'drop' | 'kiroapp'
  enabled: boolean
  base_url: string
  daily_cap_cny: number
  priority?: number
  shelf_priority?: Record<string, number>
  has_key: boolean
  /** 非空即这家已熔断，内容是原因。 */
  breaker: string
}

/** PUT 时的一家。`api_key` 缺省 = 保留原值（面板改别的字段不用知道密钥）。 */
export interface SupplierPatch {
  id: string
  kind: string
  enabled: boolean
  base_url: string
  daily_cap_cny: number
  priority: number
  shelf_priority: Record<string, number>
  api_key?: string
}
