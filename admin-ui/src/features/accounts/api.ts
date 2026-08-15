import { api } from '@/lib/api'

import type {
  AccountModelsLocalResult,
  AccountQuota,
  AccountRow,
  AccountRuntimeInstance,
  CreateAccountPayload,
  ImportAccountsPayload,
  ImportAccountsResult,
  ImportApiKeysPayload,
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

/** `POST /accounts/cursor/login/start` 入参：登记一个 Cursor 官方登录（PKCE + 轮询）会话。 */
export interface CursorLoginStartPayload {
  account_id: string
  group?: string
  /** 出口网关选择：""/"direct"=直连；"auto"=自动均衡；数字=egress_pool 索引。 */
  egress?: string
  max_concurrency?: number
  /** 建号时的调度优先级种子（缺省后端按 100=低）。 */
  priority?: number
}

export interface CursorLoginStartResult {
  login_url: string
  /** 会话标识，后续 poll 原样回传。不是密钥，但也不进 URL/日志。 */
  flow_id: string
  /** 会话有效期（秒），即前端轮询的上限窗口。 */
  expires_in_sec: number
  /** 后端建议的轮询间隔（秒）。 */
  poll_interval_sec: number
}

/** 生成 Cursor 登录链接并登记会话（纯本地，不发上游；重名 409 在此前置拦截）。 */
export async function cursorLoginStart(
  payload: CursorLoginStartPayload,
): Promise<CursorLoginStartResult> {
  const response = await api.post<CursorLoginStartResult>(
    '/accounts/cursor/login/start',
    payload,
  )
  return response.data
}

/**
 * `POST /accounts/cursor/login/poll` 的结果：
 * - 200 pending → `{ done: false }`（继续轮询）
 * - 201 done    → `{ done: true, account }`（账号已落库，body 即账号行）
 * 错误（502 瞬时 / 4xx 终态）走 axios 异常，由调用方按状态码决策。
 */
export type CursorLoginPollResult = { done: false } | { done: true; account: AccountRow }

/** 问一次「授权好了吗」。done 时会话随即失效，不可再 poll。 */
export async function cursorLoginPoll(flowId: string): Promise<CursorLoginPollResult> {
  const response = await api.post<AccountRow>('/accounts/cursor/login/poll', {
    flow_id: flowId,
  })
  if (response.status === 201) return { done: true, account: response.data }
  return { done: false }
}

export async function importApiKeys(
  payload: ImportApiKeysPayload,
): Promise<ImportAccountsResult> {
  const response = await api.post<ImportAccountsResult>('/accounts/import-apikeys', payload)
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

/** `POST /accounts/{id}/on-demand` 的结果。 */
export interface SetOnDemandResult {
  ok: boolean
  account_id: string
  /** 设置后回读的配额（含新的 on_demand）；回读失败时为 null（设置本身已成功）。 */
  quota: AccountQuota | null
}

/**
 * 设置账号的超额（on-demand）额度上限。`limitUsd = 0`/`null` = **关闭**超额。
 *
 * ⚠️ **写**操作：改的是上游账号的计费设置，开启后套餐用尽会产生真实费用。
 * 未绑支付方式的号上游会拒（"Payment method required"），错误原文会透出。
 * 目前只有 Cursor 支持。
 */
export async function setAccountOnDemand(
  id: string,
  limitUsd: number | null,
): Promise<SetOnDemandResult> {
  const response = await api.post<SetOnDemandResult>(
    `/accounts/${encodeURIComponent(id)}/on-demand`,
    { limit_usd: limitUsd },
  )
  return response.data
}

/**
 * 账号可用模型清单（`GET /accounts/{id}/models/local`）。
 * **纯本地、零上游调用**（静态目录 × 档位支持 − 已学模型标记），可以随便点；
 * 上游真相要用「拉目录 / 探针」验证。404 = 没有 worker 持有该账号。
 */
export async function getAccountModelsLocal(id: string): Promise<AccountModelsLocalResult> {
  const response = await api.get<AccountModelsLocalResult>(
    `/accounts/${encodeURIComponent(id)}/models/local`,
  )
  return response.data
}
