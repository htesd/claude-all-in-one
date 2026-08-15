import { useMutation, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import {
  createAccount,
  cursorLoginPoll,
  cursorLoginStart,
  deleteAccount,
  fetchAccounts,
  fetchAccountsRuntime,
  getAccountModelsLocal,
  importAccounts,
  importApiKeys,
  oauthComplete,
  oauthStart,
  refreshAccount,
  resetAccount,
  setAccountOnDemand,
  updateAccount,
} from './api'
import type { CursorLoginStartPayload, OAuthStartPayload } from './api'
import type {
  CreateAccountPayload,
  ImportAccountsPayload,
  ImportApiKeysPayload,
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

/**
 * 账号可用模型清单（纯本地、零上游）。传 null 即停查 ——
 * 调用方（弹窗）在关闭/无目标账号时传 null，打开时传入 account_id。
 *
 * `staleTime: 0` + `refetchOnMount: 'always'`：全局默认 staleTime 是 30s，
 * 不设的话弹窗重开会直接展示旧缓存（被标记的号可能已被救回，反之亦然），
 * 而这份数据是「查询时刻的快照」，每次打开都必须重新取（审查 gpt-5.6-sol）。
 */
export function useAccountModelsLocal(id: string | null) {
  return useQuery({
    queryKey: queryKeys.accounts.modelsLocal(id ?? ''),
    queryFn: () => getAccountModelsLocal(id as string),
    enabled: id !== null,
    staleTime: 0,
    refetchOnMount: 'always',
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

export function useImportApiKeys() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (payload: ImportApiKeysPayload) => importApiKeys(payload),
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

/** OAuth 上号第一步：生成 authorize URL（不发网络，不改账号域，无需失效查询）。 */
export function useOAuthStart() {
  return useMutation({
    mutationFn: (payload: OAuthStartPayload) => oauthStart(payload),
  })
}

/** OAuth 上号第二步：换码落库；成功后刷新账号域（新账号入列）。 */
export function useOAuthComplete() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ state, code }: { state: string; code: string }) => oauthComplete(state, code),
    onSuccess: () => invalidateAccountDomains(queryClient),
  })
}

/** Cursor 官方登录第一步：生成登录链接（纯本地，不改账号域，无需失效查询）。 */
export function useCursorLoginStart() {
  return useMutation({
    mutationFn: (payload: CursorLoginStartPayload) => cursorLoginStart(payload),
  })
}

/**
 * Cursor 官方登录轮询：问一次授权状态。pending/瞬时错误由调用方继续调度；
 * 仅 done（账号已落库）时刷新账号域让新号入列。
 */
export function useCursorLoginPoll() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (flowId: string) => cursorLoginPoll(flowId),
    onSuccess: (result) => {
      if (result.done) invalidateAccountDomains(queryClient)
    },
  })
}

/** 人工强制刷新 token（rt→at）；成功后刷新 runtime（新 token 有效期 / 配额可能随之回正）。 */
export function useRefreshAccount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => refreshAccount(id),
    onSuccess: () => invalidateAccountDomains(queryClient),
  })
}

/**
 * 设置超额（on-demand）额度上限。成功后刷新 runtime，让额度列立刻反映新上限
 * （后端已顺带回读并写了配额缓存，这里只需让前端重取）。
 */
export function useSetAccountOnDemand() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, limitUsd }: { id: string; limitUsd: number | null }) =>
      setAccountOnDemand(id, limitUsd),
    onSuccess: () => invalidateAccountDomains(queryClient),
  })
}
