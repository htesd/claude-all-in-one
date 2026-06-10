import { api } from '@/lib/api'

import type { KeyUsage, ModelUsage, UsageFilter, UsageSummary } from './types'

/** Time part of the query string. Backend priority: from/to > all > days. */
function timeParams(filter: UsageFilter): Record<string, string | number> {
  if (filter.mode === 'range' && filter.from !== undefined && filter.to !== undefined) {
    return { from: filter.from, to: filter.to }
  }
  if (filter.mode === 'all') {
    return { all: 'true' }
  }
  return { days: filter.days ?? 30 }
}

/**
 * Full params including the optional key filter.
 * Note: `key: ''` (unattributed bucket) is intentionally sent as-is — axios only
 * drops null/undefined params, so the empty string reaches the backend as `key=`.
 */
function filterParams(filter: UsageFilter): Record<string, string | number> {
  const params = timeParams(filter)
  if (filter.key !== undefined) {
    params.key = filter.key
  }
  return params
}

export async function fetchUsageSummary(filter: UsageFilter): Promise<UsageSummary> {
  const response = await api.get<UsageSummary>('/usage/summary', {
    params: filterParams(filter),
  })
  return response.data
}

export async function fetchUsageByModel(filter: UsageFilter): Promise<ModelUsage[]> {
  const response = await api.get<ModelUsage[]>('/usage/by-model', {
    params: filterParams(filter),
  })
  return response.data
}

/** by-key lists ALL keys, so only the time part of the filter applies (no `key` param). */
export async function fetchUsageByKey(filter: UsageFilter): Promise<KeyUsage[]> {
  const response = await api.get<KeyUsage[]>('/usage/by-key', {
    params: timeParams(filter),
  })
  return response.data
}
