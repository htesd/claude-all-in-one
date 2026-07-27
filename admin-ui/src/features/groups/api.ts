import { api } from '@/lib/api'

import type {
  BulkAddMembersPayload,
  CreateGroupPayload,
  GroupMember,
  GroupRow,
  UpdateGroupPayload,
} from './types'

export async function fetchGroups(): Promise<GroupRow[]> {
  const response = await api.get<GroupRow[]>('/groups')
  return response.data
}

export async function createGroup(payload: CreateGroupPayload): Promise<GroupRow> {
  const response = await api.post<GroupRow>('/groups', payload)
  return response.data
}

export async function updateGroup(name: string, patch: UpdateGroupPayload): Promise<GroupRow> {
  const response = await api.patch<GroupRow>(`/groups/${encodeURIComponent(name)}`, patch)
  return response.data
}

export async function deleteGroup(name: string): Promise<void> {
  await api.delete(`/groups/${encodeURIComponent(name)}`)
}

// ───────── 成员边(账号↔分组 N:M) ─────────

export async function fetchGroupMembers(name: string): Promise<GroupMember[]> {
  const response = await api.get<GroupMember[]>(`/groups/${encodeURIComponent(name)}/members`)
  return response.data
}

/** 加/改一条成员边(同一账号重复提交 = 改它在本组内的优先级)。 */
export async function upsertGroupMember(
  name: string,
  accountId: string,
  priority: number,
): Promise<void> {
  await api.post(`/groups/${encodeURIComponent(name)}/members`, {
    account_id: accountId,
    priority,
  })
}

export async function removeGroupMember(name: string, accountId: string): Promise<void> {
  await api.delete(
    `/groups/${encodeURIComponent(name)}/members/${encodeURIComponent(accountId)}`,
  )
}

/** 按条件批量加成员;返回新建或改动的边数。 */
export async function bulkAddGroupMembers(
  name: string,
  payload: BulkAddMembersPayload,
): Promise<number> {
  const response = await api.post<{ added_or_updated: number }>(
    `/groups/${encodeURIComponent(name)}/members/bulk`,
    payload,
  )
  return response.data.added_or_updated
}
