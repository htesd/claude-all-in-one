import { keepPreviousData, useQuery } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { fetchUsageByKey, fetchUsageByModel, fetchUsageSummary } from './api'
import type { UsageFilter } from './types'

export function useUsageSummary(filter: UsageFilter) {
  return useQuery({
    queryKey: queryKeys.usage.summary(filter),
    queryFn: () => fetchUsageSummary(filter),
    placeholderData: keepPreviousData,
  })
}

export function useUsageByModel(filter: UsageFilter) {
  return useQuery({
    queryKey: queryKeys.usage.byModel(filter),
    queryFn: () => fetchUsageByModel(filter),
    placeholderData: keepPreviousData,
  })
}

/**
 * by-key always lists every key, so the `key` part of the filter is stripped —
 * both from the request and from the query key (changing the key filter must
 * not refetch the key list).
 */
export function useUsageByKey(filter: UsageFilter) {
  const timeFilter: UsageFilter = {
    mode: filter.mode,
    days: filter.days,
    from: filter.from,
    to: filter.to,
  }
  return useQuery({
    queryKey: queryKeys.usage.byKey(timeFilter),
    queryFn: () => fetchUsageByKey(timeFilter),
    placeholderData: keepPreviousData,
  })
}
