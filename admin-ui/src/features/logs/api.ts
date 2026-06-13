import { api } from '@/lib/api'

import { LOGS_PAGE_SIZE, type LogsFilter, type LogsPage, type RequestLogDetail } from './types'

function filterParams(filter: LogsFilter): Record<string, string | number | boolean> {
  const params: Record<string, string | number | boolean> = {}

  // Time range: 'all' → all=true, otherwise days=N
  if (filter.timeRange === 'all') {
    params.all = true
  } else {
    params.days = filter.timeRange
  }

  // Success filter: 'ok' → success=true, 'fail' → success=false, 'all' → omit
  if (filter.success === 'ok') {
    params.success = true
  } else if (filter.success === 'fail') {
    params.success = false
  }

  if (filter.account) {
    params.account = filter.account
  }
  if (filter.model) {
    params.model = filter.model
  }

  return params
}

export async function fetchLogs(filter: LogsFilter, page: number): Promise<LogsPage> {
  const response = await api.get<LogsPage>('/logs', {
    params: { ...filterParams(filter), page, page_size: LOGS_PAGE_SIZE },
  })
  return response.data
}

export async function fetchLogDetail(id: number): Promise<RequestLogDetail> {
  const response = await api.get<RequestLogDetail>(`/logs/${id}`)
  return response.data
}
