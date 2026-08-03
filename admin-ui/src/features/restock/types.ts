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

export interface RestockSnapshot {
  healthy?: number
  cooling?: number
  dead?: number
  total?: number
  any_online?: boolean
  stock?: number
  price_usd?: number
  balance_cny?: number
  stock_at?: number
  at?: number
  drop_ok?: boolean
  drop_error?: string
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
  unit_cost: number | null
}

export interface RestockAccountsResponse {
  count: number
  items: RestockAccount[]
}
