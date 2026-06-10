import { api } from '@/lib/api'

import type {
  AccountRow,
  AccountRuntimeInstance,
  CreateAccountPayload,
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
