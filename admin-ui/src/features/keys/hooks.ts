import { useMutation, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { createKey, deleteKey, fetchKeys, updateKey } from './api'
import type { CreateKeyPayload, UpdateKeyPayload } from './types'

export function useKeys() {
  return useQuery({
    queryKey: queryKeys.keys.list(),
    queryFn: fetchKeys,
  })
}

/** Key 增删改后：刷新 key 域 + 分组域（分组归属变化影响分组卡片的 key_count）。 */
function invalidateKeyDomains(queryClient: QueryClient) {
  void queryClient.invalidateQueries({ queryKey: queryKeys.keys.root })
  void queryClient.invalidateQueries({ queryKey: queryKeys.groups.root })
}

export function useCreateKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: CreateKeyPayload) => createKey(payload),
    onSuccess: () => invalidateKeyDomains(queryClient),
  })
}

export function useUpdateKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ key, patch }: { key: string; patch: UpdateKeyPayload }) =>
      updateKey(key, patch),
    onSuccess: () => invalidateKeyDomains(queryClient),
  })
}

export function useDeleteKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (key: string) => deleteKey(key),
    onSuccess: () => {
      // 删除只移除 api_keys 行；用量仍按原 client_key_id 聚合、数据不变，无需刷新 usage 域
      invalidateKeyDomains(queryClient)
    },
  })
}
