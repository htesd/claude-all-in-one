export type LogsSuccessFilter = 'all' | 'ok' | 'fail'
export type LogsTimeRange = 7 | 30 | 'all'

export interface LogsFilter {
  success: LogsSuccessFilter
  timeRange: LogsTimeRange
  account?: string
  model?: string
}

/** 每页条数(前端固定;与后端 DEFAULT_PAGE_SIZE 对齐)。 */
export const LOGS_PAGE_SIZE = 50

/** 列表分页响应信封(后端 GET /logs 返回)。 */
export interface LogsPage {
  items: RequestLogRow[]
  total: number
  page: number
  page_size: number
}

export interface RequestLogRow {
  id: number
  created_at: number
  client_key_id: string
  account_id: string
  model: string
  stream: boolean
  success: boolean
  status_code: number | null
  error_kind: string | null
  duration_ms: number | null
  ttfb_ms: number | null
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  reported_tokens: number
  /** 真:上游真实 cacheReadInputTokens(0=miss/无信号)。 */
  real_cache_read_tokens: number
  /** credit:Kiro meteringEvent.usage(真号本次真实计费;0=无信号)。 */
  metering_credit: number
}

/** 从报文抽出的去重媒体(图片/文档)。报文里以 `blob:<hash>` 引用。 */
export interface LogBlob {
  hash: string
  media_type: string
  /** base64 原文。 */
  data: string
  bytes: number
}

export interface RequestLogDetail extends RequestLogRow {
  client_payload: string
  kiro_payload: string
  /** 本条日志报文引用到的去重媒体 blob。 */
  blobs: LogBlob[]
}
