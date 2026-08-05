import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import {
  buyNow,
  fetchRestockAccounts,
  fetchRestockCredits,
  fetchRestockParams,
  fetchRestockState,
  fetchSuppliers,
  resetBreaker,
  updateRestockParams,
  updateSuppliers,
} from './api'
import type { RestockParams, SupplierPatch } from './types'

/** 实况。15s 轮询，与账号运行态同一节奏。 */
export function useRestockState() {
  return useQuery({
    queryKey: queryKeys.restock.state(),
    queryFn: fetchRestockState,
    refetchInterval: 15_000,
  })
}

export function useRestockParams() {
  return useQuery({
    queryKey: queryKeys.restock.params(),
    queryFn: fetchRestockParams,
  })
}

/** 改参数；成功后整域 invalidate（开关/窗口会立刻反映到实况卡）。 */
export function useUpdateRestockParams() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (patch: Partial<RestockParams>) => updateRestockParams(patch),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.restock.root })
    },
  })
}

/** 货源名册。改动不频繁，跟着实况一起刷新即可，不单独轮询。 */
export function useSuppliers() {
  return useQuery({
    queryKey: queryKeys.restock.suppliers(),
    queryFn: fetchSuppliers,
  })
}

/** 改名册；成功后整域 invalidate（实况卡里的逐家视图要立刻跟上）。 */
export function useUpdateSuppliers() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (items: SupplierPatch[]) => updateSuppliers(items),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.restock.root })
    },
  })
}

/**
 * 积分曲线。**不轮询** —— 这个接口要跑聚合，而小时级数据不需要秒级刷新；
 * 手动切时间范围时自然会重新拉。
 */
export function useRestockCredits(hours: number) {
  return useQuery({
    queryKey: queryKeys.restock.credits(hours),
    queryFn: () => fetchRestockCredits(hours),
    staleTime: 60_000,
  })
}

export function useRestockAccounts() {
  return useQuery({
    queryKey: queryKeys.restock.accounts(),
    queryFn: fetchRestockAccounts,
    refetchInterval: 30_000,
  })
}

export function useBuyNow() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: buyNow,
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.restock.root })
    },
  })
}

export function useResetBreaker() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: resetBreaker,
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.restock.root })
    },
  })
}
