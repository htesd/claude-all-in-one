import { api } from '@/lib/api'

import type { KeyUsage, ModelUsage, TimeRange, UsageSummary } from './types'

function rangeParams(range: TimeRange): Record<string, string | number> {
  return range === 'all' ? { all: 'true' } : { days: range }
}

export async function fetchUsageSummary(range: TimeRange): Promise<UsageSummary> {
  const response = await api.get<UsageSummary>('/usage/summary', {
    params: rangeParams(range),
  })
  return response.data
}

export async function fetchUsageByModel(range: TimeRange): Promise<ModelUsage[]> {
  const response = await api.get<ModelUsage[]>('/usage/by-model', {
    params: rangeParams(range),
  })
  return response.data
}

export async function fetchUsageByKey(range: TimeRange): Promise<KeyUsage[]> {
  const response = await api.get<KeyUsage[]>('/usage/by-key', {
    params: rangeParams(range),
  })
  return response.data
}
