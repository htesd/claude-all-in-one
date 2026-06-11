import { api } from '@/lib/api'

import type {
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

/** 人工救号：清 worker 内存里的运行时禁用/冷却/失败计数（配置层 disabled 不动）。 */
export async function resetAccount(id: string): Promise<void> {
  await api.post(`/accounts/${encodeURIComponent(id)}/reset`)
}
