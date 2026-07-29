import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { applyMembershipDiff, createGroup, deleteGroup, fetchGroups, updateGroup } from './api'
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

/**
 * 一次保存一个账号的**全部**成员边改动（先加后删，见 `applyMembershipDiff`）。
 *
 * 刻意不做成"每条边一个 mutation"：那样改 N 个组就会触发 N×2 次 invalidate，
 * 请求陆续返回时账号列表被反复拉取，中途还会闪现半成品拓扑。这里全部 settle 后
 * 只失效一次。用 `onSettled` 而非 `onSuccess`——部分失败时同样要拿到权威的新基线，
 * 否则用户重试时差集算的还是旧账。
 *
 * 成员边直接决定这个号对哪些客户可见，所以**账号域**也必须失效，
 * 只刷分组域的话账号页那几个组名 chip 会停在旧值上。
 */
export function useSaveMemberships() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({
      accountId,
      upserts,
      removals,
    }: {
      accountId: string
      upserts: { name: string; priority: number }[]
      removals: string[]
    }) => applyMembershipDiff(accountId, upserts, removals),
    onSettled: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.groups.root })
      void queryClient.invalidateQueries({ queryKey: queryKeys.accounts.root })
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
