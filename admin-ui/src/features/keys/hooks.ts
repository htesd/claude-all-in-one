import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { createKey, deleteKey, fetchKeys, updateKey } from './api'
import type { CreateKeyPayload, UpdateKeyPayload } from './types'

export function useKeys() {
  return useQuery({
    queryKey: queryKeys.keys.list(),
    queryFn: fetchKeys,
  })
}

export function useCreateKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: CreateKeyPayload) => createKey(payload),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.keys.root })
    },
  })
}

export function useUpdateKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ key, patch }: { key: string; patch: UpdateKeyPayload }) =>
      updateKey(key, patch),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.keys.root })
    },
  })
}

export function useDeleteKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (key: string) => deleteKey(key),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.keys.root })
      // 删除后该 key 的历史流量在用量端的归属展示口径可能变化（如归入"未归属"），联动刷新
      void queryClient.invalidateQueries({ queryKey: queryKeys.usage.root })
    },
  })
}
