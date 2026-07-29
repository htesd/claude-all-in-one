import { api, extractErrorMessage, getErrorStatus } from '@/lib/api'

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

/**
 * 把一个账号的成员边差集落库。**先加后删**,且加失败就完全不删。
 *
 * 后端没有事务型的批量端点,一次保存必然是多个独立请求。顺序因此是安全性的唯一抓手:
 * 若先删后加、而新增那条因 CrossOwner / 网络 / 5xx 失败,账号会当场变成"分组比原来还少"
 * ——极端情况下一个组都不剩,付费客户直接掉容量。先加后删则最坏只是多挂了几个组,
 * 可见、可恢复、不掉容量。
 *
 * 删除对 404 宽容:边本来就不在 = 目标状态已达成。没有这条,部分失败后重试会因为
 * "上一轮已经删成功、账号列表还没 refetch 完"而重复 DELETE,永远卡在报错上。
 */
export async function applyMembershipDiff(
  accountId: string,
  upserts: { name: string; priority: number }[],
  removals: string[],
): Promise<{ failures: string[] }> {
  const added = await Promise.allSettled(
    upserts.map((m) => upsertGroupMember(m.name, accountId, m.priority)),
  )
  const addFailures = added.flatMap((r, i) =>
    r.status === 'rejected' ? [`${upserts[i].name}: ${extractErrorMessage(r.reason)}`] : [],
  )
  // 加失败就此打住 —— 一条边都不删,账号保有原有全部分组。
  if (addFailures.length > 0) return { failures: addFailures }

  const removed = await Promise.allSettled(
    removals.map((name) => removeGroupMember(name, accountId)),
  )
  return {
    failures: removed.flatMap((r, i) => {
      if (r.status !== 'rejected') return []
      if (getErrorStatus(r.reason) === 404) return []
      return [`${removals[i]}: ${extractErrorMessage(r.reason)}`]
    }),
  }
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
