import { api } from '@/lib/api'

import type {
  AccountQuota,
  AccountRow,
  AccountRuntimeInstance,
  CreateAccountPayload,
  ImportAccountsPayload,
  ImportAccountsResult,
  UpdateAccountPayload,
} from './types'

export async function fetchAccounts(): Promise<AccountRow[]> {
  const response = await api.get<AccountRow[]>('/accounts')
  return response.data
}

export async function fetchAccountsRuntime(): Promise<AccountRuntimeInstance[]> {
  const response = await api.get<AccountRuntimeInstance[]>('/accounts/runtime')
  return response.data
}

export async function createAccount(payload: CreateAccountPayload): Promise<AccountRow> {
  const response = await api.post<AccountRow>('/accounts', payload)
  return response.data
}

export async function updateAccount(
  id: string,
  patch: UpdateAccountPayload,
): Promise<AccountRow> {
  const response = await api.patch<AccountRow>(`/accounts/${encodeURIComponent(id)}`, patch)
  return response.data
}

export async function deleteAccount(id: string): Promise<void> {
  await api.delete(`/accounts/${encodeURIComponent(id)}`)
}

export async function importAccounts(
  payload: ImportAccountsPayload,
): Promise<ImportAccountsResult> {
  const response = await api.post<ImportAccountsResult>('/accounts/import', payload)
  return response.data
}

/** `POST /accounts/oauth/start` 入参：生成 authorize URL 并登记待完成会话。 */
export interface OAuthStartPayload {
  account_id: string
  group?: string
  /** 出口网关选择：""/"direct"=直连；"auto"=自动均衡；数字=egress_pool 索引。 */
  egress?: string
  max_concurrency?: number
}

export interface OAuthStartResult {
  authorize_url: string
  /** 会话键 + CSRF 绑定；complete 时原样回传。 */
  state: string
  expires_in_sec: number
}

/** 生成 PKCE + authorize URL（不发任何网络），登记待完成上号会话。 */
export async function oauthStart(payload: OAuthStartPayload): Promise<OAuthStartResult> {
  const response = await api.post<OAuthStartResult>('/accounts/oauth/start', payload)
  return response.data
}

/** 用 code 换 token（扇给目标组 worker，走该组 egress）并落库，返回新账号（脱敏）。 */
export async function oauthComplete(state: string, code: string): Promise<AccountRow> {
  const response = await api.post<AccountRow>('/accounts/oauth/complete', { state, code })
  return response.data
}

/** 人工救号：清 worker 内存里的运行时禁用/冷却/失败计数（配置层 disabled 不动）。 */
export async function resetAccount(id: string): Promise<void> {
  await api.post(`/accounts/${encodeURIComponent(id)}/reset`)
}

/** `POST /accounts/{id}/refresh` 的结果：强制刷新 token 成功后回新 access_token 的有效期。 */
export interface RefreshAccountResult {
  refreshed: boolean
  account_id: string
  /** 新 access_token 过期时刻（RFC3339）；上游未给则 null。绝不回传 token 明文。 */
  expires_at: string | null
}

/**
 * 人工强制刷新该账号 token（rt→at 换一次）。这是后台轮询本就在做的 OIDC 交换，
 * **不是** chat，不触发风控；可用于验证 refresh_token 仍可用 / 轮换 rt 后立即生效。
 */
export async function refreshAccount(id: string): Promise<RefreshAccountResult> {
  const response = await api.post<RefreshAccountResult>(
    `/accounts/${encodeURIComponent(id)}/refresh`,
  )
  return response.data
}

/** `POST /accounts/{id}/quota` 的结果：按需验活（刷新 token + 查配额，只读）。 */
export interface VerifyQuotaResult {
  verified: boolean
  account_id: string
  /** 配额快照；verified=false 且为 null = 账号可刷新但上游无配额数据。 */
  quota: AccountQuota | null
}

/**
 * 按需验活：让持有方 worker 确保 token 有效（必要时刷新）并查一次配额
 * （getUsageLimits）。全程只读、**绝不发 chat**；死号在此处现形（403/invalid_grant）。
 */
export async function fetchAccountQuotaNow(id: string): Promise<VerifyQuotaResult> {
  const response = await api.post<VerifyQuotaResult>(
    `/accounts/${encodeURIComponent(id)}/quota`,
  )
  return response.data
}
