import { keepPreviousData, useQuery } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { fetchLogDetail, fetchLogs } from './api'
import type { LogsFilter } from './types'

export function useLogs(filter: LogsFilter, page: number) {
  return useQuery({
    queryKey: queryKeys.logs.list(filter, page),
    queryFn: () => fetchLogs(filter, page),
    placeholderData: keepPreviousData,
    refetchInterval: 10_000,
  })
}

export function useLogDetail(id: number | null) {
  return useQuery({
    queryKey: queryKeys.logs.detail(id ?? 0),
    queryFn: () => fetchLogDetail(id!),
    enabled: id !== null,
  })
}
