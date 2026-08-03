import { describe, expect, it } from 'vitest'

import { diffMemberships, sortAccounts } from './lib'
import type { AccountGroupMembership, AccountRow } from './types'

/** 只填排序用得到的字段，其余给最小合法值。 */
function row(account_id: string, created_at: number, group_name = 'G0'): AccountRow {
  return {
    account_id,
    group_name,
    provider: 'kiro',
    max_concurrency: 1,
    priority: 100,
    disabled: false,
    extra: {},
    created_at,
  }
}

describe('账号列表排序', () => {
  it('默认「最新在前」：按 created_at 倒序', () => {
    const rows = [row('a', 100), row('b', 300), row('c', 200)]
    expect(sortAccounts(rows, 'created_desc').map((r) => r.account_id)).toEqual(['b', 'c', 'a'])
  })

  it('「最早在前」：按 created_at 正序', () => {
    const rows = [row('a', 100), row('b', 300), row('c', 200)]
    expect(sortAccounts(rows, 'created_asc').map((r) => r.account_id)).toEqual(['a', 'c', 'b'])
  })

  it('同批导入(created_at 相同)按 id 兜底，顺序稳定', () => {
    // Kiro 号成批上，同批的 created_at 是同一秒。没有兜底键的话，
    // 15s 运行态轮询每刷新一次同批号就会互相跳位。
    const rows = [row('k3', 500), row('k1', 500), row('k2', 500)]
    expect(sortAccounts(rows, 'created_desc').map((r) => r.account_id)).toEqual(['k1', 'k2', 'k3'])
    expect(sortAccounts(rows, 'created_asc').map((r) => r.account_id)).toEqual(['k1', 'k2', 'k3'])
  })

  it('「按组+名称」复现后端原序(group_name ASC, account_id ASC)', () => {
    const rows = [row('b', 1, 'GLOW'), row('a', 2, 'GLOW'), row('z', 3, 'G0')]
    expect(sortAccounts(rows, 'name').map((r) => r.account_id)).toEqual(['z', 'a', 'b'])
  })

  it('不修改入参数组——原地 sort 会污染 react-query 缓存', () => {
    const rows = [row('a', 100), row('b', 300)]
    const before = rows.map((r) => r.account_id)
    sortAccounts(rows, 'created_desc')
    expect(rows.map((r) => r.account_id)).toEqual(before)
  })

  it('空列表不炸', () => {
    expect(sortAccounts([], 'created_desc')).toEqual([])
  })
})

const HIGH = 0
const LOW = 100

describe('成员边差集(编辑账号时决定发哪些请求)', () => {
  it('没动过 → 一个请求都不发', () => {
    const current: AccountGroupMembership[] = [
      { name: 'G0', priority: LOW },
      { name: 'GLOW', priority: HIGH },
    ]
    // 故意换一份新数组(引用不同、内容相同),证明比的是内容不是引用。
    const diff = diffMemberships(current, [
      { name: 'G0', priority: LOW },
      { name: 'GLOW', priority: HIGH },
    ])
    expect(diff).toEqual({ upserts: [], removals: [] })
  })

  it('新勾一个组 → 只 upsert 那一个,已有的组不重发', () => {
    const diff = diffMemberships(
      [{ name: 'G0', priority: LOW }],
      [
        { name: 'G0', priority: LOW },
        { name: 'GECO', priority: LOW },
      ],
    )
    expect(diff.upserts).toEqual([{ name: 'GECO', priority: LOW }])
    expect(diff.removals).toEqual([])
  })

  it('取消勾选 → 只删那一条边', () => {
    const diff = diffMemberships(
      [
        { name: 'G0', priority: LOW },
        { name: 'GLOW', priority: HIGH },
      ],
      [{ name: 'G0', priority: LOW }],
    )
    expect(diff.upserts).toEqual([])
    expect(diff.removals).toEqual(['GLOW'])
  })

  it('只改组内档位 → 走 upsert(后端同一个调用),不产生删除', () => {
    const diff = diffMemberships(
      [{ name: 'GLOW', priority: LOW }],
      [{ name: 'GLOW', priority: HIGH }],
    )
    expect(diff.upserts).toEqual([{ name: 'GLOW', priority: HIGH }])
    expect(diff.removals).toEqual([])
  })

  it('加、删、改档位同时发生 → 三类各归各位', () => {
    const diff = diffMemberships(
      [
        { name: 'G0', priority: LOW },
        { name: 'GECO', priority: LOW },
        { name: 'DROPME', priority: LOW },
      ],
      [
        { name: 'G0', priority: LOW }, // 不动
        { name: 'GECO', priority: HIGH }, // 改档位
        { name: 'GLOW', priority: HIGH }, // 新加
      ],
    )
    expect(diff.upserts).toEqual([
      { name: 'GECO', priority: HIGH },
      { name: 'GLOW', priority: HIGH },
    ])
    expect(diff.removals).toEqual(['DROPME'])
    expect(diff.upserts.map((m) => m.name)).not.toContain('G0')
  })

  it('从"一个组都没有"开始 → 全是新增,没有删除', () => {
    // 这是 2026-07-29 那种形态:号在库里、不在任何组,谁也用不到。
    const diff = diffMemberships([], [{ name: 'GLOW', priority: HIGH }])
    expect(diff.upserts).toEqual([{ name: 'GLOW', priority: HIGH }])
    expect(diff.removals).toEqual([])
  })

  it('库里是 0/100 之外的历史值、用户没碰过该组 → 不得被"规范化"成 0/100', () => {
    // 后端 priority 是任意 i64。若草稿只存高/低两档、提交时再映射回 0/100,
    // 一条优先级 50 的边会在运维只改并发时被静默改写成 0。草稿存原始数值才不会。
    const current: AccountGroupMembership[] = [{ name: 'GECO', priority: 50 }]
    const diff = diffMemberships(current, [{ name: 'GECO', priority: 50 }])
    expect(diff.upserts).toEqual([])
    expect(diff.removals).toEqual([])
  })

  it('用户真的点了档位 → 历史值才被改写', () => {
    const diff = diffMemberships(
      [{ name: 'GECO', priority: 50 }],
      [{ name: 'GECO', priority: HIGH }],
    )
    expect(diff.upserts).toEqual([{ name: 'GECO', priority: HIGH }])
  })

  it('全部取消 → 全是删除,没有新增', () => {
    const diff = diffMemberships(
      [
        { name: 'G0', priority: LOW },
        { name: 'GLOW', priority: HIGH },
      ],
      [],
    )
    expect(diff.upserts).toEqual([])
    expect(diff.removals).toEqual(['G0', 'GLOW'])
  })
})
