import { useMutation, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import {
  createAccount,
  deleteAccount,
  fetchAccounts,
  fetchAccountsRuntime,
  importAccounts,
  resetAccount,
  updateAccount,
} from './api'
import type {
  CreateAccountPayload,
  ImportAccountsPayload,
  UpdateAccountPayload,
} from './types'

export function useAccounts() {
  return useQuery({
    queryKey: queryKeys.accounts.list(),
    queryFn: fetchAccounts,
  })
}

/** worker 运行态，15s 轮询刷新（冷却倒计时 / 在线状态）。 */
export function useAccountsRuntime() {
  return useQuery({
    queryKey: queryKeys.accounts.runtime(),
    queryFn: fetchAccountsRuntime,
    refetchInterval: 15_000,
  })
}

/** 账号增删改后：刷新账号域（含 runtime）+ 分组域（account_count 计数）。 */
function invalidateAccountDomains(queryClient: QueryClient) {
  void queryClient.invalidateQueries({ queryKey: queryKeys.accounts.root })
  void queryClient.invalidateQueries({ queryKey: queryKeys.groups.root })
}

export function useCreateAccount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: CreateAccountPayload) => createAccount(payload),
    onSuccess: () => invalidateAccountDomains(queryClient),
  })
}

export function useUpdateAccount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, patch }: { id: string; patch: UpdateAccountPayload }) =>
      updateAccount(id, patch),
    onSuccess: () => invalidateAccountDomains(queryClient),
  })
}

export function useDeleteAccount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => deleteAccount(id),
    onSuccess: () => invalidateAccountDomains(queryClient),
  })
}

export function useImportAccounts() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: ImportAccountsPayload) => importAccounts(payload),
    onSuccess: () => invalidateAccountDomains(queryClient),
  })
}

/** 人工救号（清冷却/封禁/失败计数）；成功后立刻刷新 runtime 让状态列回正。 */
export function useResetAccount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => resetAccount(id),
    onSuccess: () => invalidateAccountDomains(queryClient),
  })
}
