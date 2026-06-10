/** Time range for usage queries: last N days or all time. */
export type TimeRange = 7 | 30 | 'all'

export interface UsageSummary {
  requests: number
  success_requests: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
}

export interface ModelUsage {
  model: string
  requests: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
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
}
