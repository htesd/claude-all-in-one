import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { createGroup, deleteGroup, fetchGroups, updateGroup } from './api'
import type { CreateGroupPayload, UpdateGroupPayload } from './types'

export function useGroups() {
  return useQuery({
    queryKey: queryKeys.groups.list(),
    queryFn: fetchGroups,
  })
}

export function useCreateGroup() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: CreateGroupPayload) => createGroup(payload),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.groups.root })
    },
  })
}

export function useUpdateGroup() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ name, patch }: { name: string; patch: UpdateGroupPayload }) =>
      updateGroup(name, patch),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.groups.root })
    },
  })
}

export function useDeleteGroup() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (name: string) => deleteGroup(name),
    onSuccess: () => {
      // 删除分组会把成员账号/Key 的 group_name 清空（不级联删），相关列表需一并刷新
      void queryClient.invalidateQueries({ queryKey: queryKeys.groups.root })
      void queryClient.invalidateQueries({ queryKey: queryKeys.accounts.root })
      void queryClient.invalidateQueries({ queryKey: queryKeys.keys.root })
    },
  })
}
