import { keepPreviousData, useQuery } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { fetchUsageByKey, fetchUsageByModel, fetchUsageSummary } from './api'
import type { TimeRange } from './types'

export function useUsageSummary(range: TimeRange) {
  return useQuery({
    queryKey: queryKeys.usage.summary(range),
    queryFn: () => fetchUsageSummary(range),
    placeholderData: keepPreviousData,
  })
}

export function useUsageByModel(range: TimeRange) {
  return useQuery({
    queryKey: queryKeys.usage.byModel(range),
    queryFn: () => fetchUsageByModel(range),
    placeholderData: keepPreviousData,
  })
}

export function useUsageByKey(range: TimeRange) {
  return useQuery({
    queryKey: queryKeys.usage.byKey(range),
    queryFn: () => fetchUsageByKey(range),
    placeholderData: keepPreviousData,
  })
}
