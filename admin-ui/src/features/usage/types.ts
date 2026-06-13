/** Quick time-range presets for usage queries: last N days or all time. */
export type TimeRange = 7 | 30 | 'all'

/**
 * Usage query filter, mirroring the backend contract (priority: from/to > all > days).
 * - mode 'preset' : last `days` days
 * - mode 'all'    : full history
 * - mode 'range'  : custom [from, to) range in unix SECONDS (`to` exclusive)
 *
 * `key` narrows summary / by-model to one client key id:
 * - undefined => all keys (param omitted)
 * - ''        => the unattributed bucket
 */
export interface UsageFilter {
  mode: 'preset' | 'range' | 'all'
  days?: number
  from?: number
  to?: number
  key?: string
}

/** Convert a quick preset to a UsageFilter (no key filter). */
export function rangeToFilter(range: TimeRange): UsageFilter {
  return range === 'all' ? { mode: 'all' } : { mode: 'preset', days: range }
}

export type CostBasis = 'billed' | 'real'

export interface UsageSummary {
  requests: number
  success_requests: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  real_cache_read_tokens: number
  metering_credit: number
  cost_billed_usd: number
  cost_real_usd: number
  unpriced_requests: number
}

export interface ModelUsage {
  model: string
  requests: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  real_cache_read_tokens: number
  metering_credit: number
  cost_billed_usd: number
  cost_real_usd: number
  priced: boolean
}

export interface KeyUsage {
  /** May be an empty string => unattributed traffic. */
  client_key_id: string
  requests: number
  success_requests: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  real_cache_read_tokens: number
  metering_credit: number
}
