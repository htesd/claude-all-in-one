import { api } from '@/lib/api'

import type { CreateGroupPayload, GroupRow, UpdateGroupPayload } from './types'

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
