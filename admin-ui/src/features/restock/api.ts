import { api } from '@/lib/api'

import type {
  RestockAccountsResponse,
  RestockCredits,
  RestockParams,
  RestockParamsResponse,
  RestockState,
  SupplierConfig,
  SupplierPatch,
} from './types'

/** GET /admin/api/restock/state → 实况（开关、水位、库存余额、决策流水）。 */
export async function fetchRestockState(): Promise<RestockState> {
  const response = await api.get<RestockState>('/restock/state')
  return response.data
}

/** GET /admin/api/restock/params → 参数当前值 + 上下限规格。 */
export async function fetchRestockParams(): Promise<RestockParamsResponse> {
  const response = await api.get<RestockParamsResponse>('/restock/params')
  return response.data
}

/** PUT /admin/api/restock/params → 部分更新，即时生效无需重启。 */
export async function updateRestockParams(
  patch: Partial<RestockParams>,
): Promise<{ values: RestockParams }> {
  const response = await api.put<{ values: RestockParams }>('/restock/params', patch)
  return response.data
}

/** GET /admin/api/restock/suppliers → 货源名册（**不含密钥**，只有 has_key）。 */
export async function fetchSuppliers(): Promise<{ items: SupplierConfig[] }> {
  const response = await api.get<{ items: SupplierConfig[] }>('/restock/suppliers')
  return response.data
}

/**
 * PUT /admin/api/restock/suppliers → 整份替换名册。
 *
 * 补货循环**下一轮自动重建客户端，不用重启** —— 断供时能立刻加一家，
 * 而那正是多供应商最要紧的动作。
 */
export async function updateSuppliers(items: SupplierPatch[]): Promise<{ items: SupplierConfig[] }> {
  const response = await api.put<{ items: SupplierConfig[] }>('/restock/suppliers', { items })
  return response.data
}

/** GET /admin/api/restock/credits → 积分曲线 + 按周预测 + 画像。 */
export async function fetchRestockCredits(hours: number): Promise<RestockCredits> {
  const response = await api.get<RestockCredits>('/restock/credits', { params: { hours } })
  return response.data
}

/** GET /admin/api/restock/accounts → ksk_ 号清单（按创建时间倒序）。 */
export async function fetchRestockAccounts(): Promise<RestockAccountsResponse> {
  const response = await api.get<RestockAccountsResponse>('/restock/accounts')
  return response.data
}

/** POST /admin/api/restock/buy-now → 手动补一个。**仍受花钱闸门约束**。 */
export async function buyNow(): Promise<{ act: boolean; message: string }> {
  const response = await api.post<{ act: boolean; message: string }>('/restock/buy-now')
  return response.data
}

/** POST /admin/api/restock/reset-breaker → 解除熔断。 */
export async function resetBreaker(): Promise<void> {
  await api.post('/restock/reset-breaker')
}
