import { describe, expect, it } from 'vitest'

import {
  accountStatusBucket,
  buildCursorExtra,
  buildProviderTabs,
  decideCursorLoginPoll,
  deriveAccountStatus,
  deriveModelAvailability,
  diffMemberships,
  formatMarkTtl,
  formatUsd,
  isOnDemandHigh,
  modelAllowlistAllows,
  onDemandState,
  providerTabLabel,
  quotaKindForProvider,
  sortAccounts,
} from './lib'
import type { AccountGroupMembership, AccountModelEntry, AccountRow, AccountRuntimeEntry } from './types'
import type { AccountUnavailableReason, OnDemandQuota } from './types'

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

describe('provider 展示标签(筛选器选项与表格 provider 列共用)', () => {
  it('已知 provider 映射到用户惯用短标签', () => {
    expect(providerTabLabel('kiro')).toBe('Kiro')
    expect(providerTabLabel('claude-dario')).toBe('ccmax')
    expect(providerTabLabel('cursor')).toBe('Cursor')
  })

  it('空 provider 显示占位符,未知 provider 原样返回', () => {
    expect(providerTabLabel('')).toBe('—')
    expect(providerTabLabel('claude-subprocess')).toBe('claude-subprocess')
  })
})

describe('provider 配额口径(决定账号页配额列语义)', () => {
  it('kiro = 积分,claude-dario = 5h/7d 窗口,cursor = 官方美元额度(同 credits 列)', () => {
    expect(quotaKindForProvider('kiro')).toBe('credits')
    expect(quotaKindForProvider('claude-dario')).toBe('windows')
    expect(quotaKindForProvider('cursor')).toBe('credits')
  })

  it('未知 provider 回落积分口径(历史行为)', () => {
    expect(quotaKindForProvider('')).toBe('credits')
    expect(quotaKindForProvider('claude-subprocess')).toBe('credits')
  })
})

describe('模型白名单判定(镜像后端 gw-core::account::model_allowlist_allows,改语义必须两边同步)', () => {
  it('null / 缺失 = 不限,放行一切', () => {
    expect(modelAllowlistAllows(null, 'claude-sonnet-5')).toBe(true)
    expect(modelAllowlistAllows(undefined, 'anything')).toBe(true)
  })

  it('空数组 = 全禁(fail-closed;写侧正常不会存出这个形态)', () => {
    expect(modelAllowlistAllows([], 'default')).toBe(false)
  })

  it('精确条目小写匹配,大小写不敏感', () => {
    expect(modelAllowlistAllows(['default', 'composer-2.5'], 'default')).toBe(true)
    expect(modelAllowlistAllows(['default'], 'Default')).toBe(true)
    expect(modelAllowlistAllows(['default'], 'claude-sonnet-5')).toBe(false)
  })

  it('「前缀*」通配仅匹配前缀;全名精确条目不吃前缀', () => {
    expect(modelAllowlistAllows(['grok*'], 'grok-4.6')).toBe(true)
    expect(modelAllowlistAllows(['grok*'], 'kimi-k3')).toBe(false)
    // 无星号的全名条目是精确匹配,不能当前缀用
    expect(modelAllowlistAllows(['grok'], 'grok-4.6')).toBe(false)
  })
})

describe('Cursor 建号 extra 组装(字段名与后端 CURSOR_ACCOUNT_SCHEMA 一一对应)', () => {
  it('只填必填项：extra 只有 access_token(且 trim)', () => {
    expect(buildCursorExtra({ access_token: '  jwt.x.y  ' })).toEqual({ access_token: 'jwt.x.y' })
  })

  it('可选字段 trim 后为空一律省略——空串会顶掉后端「留空 = 派生/默认」语义', () => {
    const extra = buildCursorExtra({
      access_token: 'jwt',
      refresh_token: '   ',
      machine_id: '',
      mac_machine_id: undefined,
      config_version: ' ',
      timezone: '',
      proxy: '  ',
    })
    expect(extra).toEqual({ access_token: 'jwt' })
  })

  it('填了的可选字段保留并 trim', () => {
    const extra = buildCursorExtra({
      access_token: 'jwt',
      refresh_token: ' rt ',
      machine_id: ' a'.repeat(1) + 'b'.repeat(63),
      timezone: ' America/Los_Angeles ',
      proxy: ' socks5://127.0.0.1:1080 ',
    })
    expect(extra).toEqual({
      access_token: 'jwt',
      refresh_token: 'rt',
      machine_id: `a${'b'.repeat(63)}`,
      timezone: 'America/Los_Angeles',
      proxy: 'socks5://127.0.0.1:1080',
    })
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

describe('provider 筛选项', () => {
  /** 指定 provider 的行（其余字段取最小合法值）。 */
  function pRow(account_id: string, provider: string): AccountRow {
    return { ...row(account_id, 1), provider }
  }

  it('库里一个 cursor 号都没有时，Cursor 标签仍然出现（计数 0）', () => {
    // 2026-08-09 线上就是这个形态：kiro=380 / claude-dario=2 / cursor=0。
    // 旧实现按现有账号 GROUP BY，Cursor 标签不出现，看起来像"部署缺了东西"，
    // 而"能不能上第一个 cursor 号"恰恰是要验证的第一件事。
    const tabs = buildProviderTabs([pRow('k1', 'kiro'), pRow('d1', 'claude-dario')])
    expect(tabs).toEqual([
      { provider: 'kiro', count: 1 },
      { provider: 'claude-dario', count: 1 },
      { provider: 'cursor', count: 0 },
    ])
  })

  it('库里完全没有账号时，三个已知 provider 全部常驻', () => {
    expect(buildProviderTabs([]).map((t) => t.provider)).toEqual([
      'kiro',
      'claude-dario',
      'cursor',
    ])
    expect(buildProviderTabs(undefined).every((t) => t.count === 0)).toBe(true)
  })

  it('顺序固定 kiro→ccmax→cursor，与输入顺序无关', () => {
    const tabs = buildProviderTabs([pRow('c1', 'cursor'), pRow('k1', 'kiro')])
    expect(tabs.map((t) => t.provider)).toEqual(['kiro', 'claude-dario', 'cursor'])
  })

  it('计数正确累加', () => {
    const tabs = buildProviderTabs([
      pRow('k1', 'kiro'),
      pRow('k2', 'kiro'),
      pRow('c1', 'cursor'),
    ])
    expect(tabs.find((t) => t.provider === 'kiro')?.count).toBe(2)
    expect(tabs.find((t) => t.provider === 'cursor')?.count).toBe(1)
    expect(tabs.find((t) => t.provider === 'claude-dario')?.count).toBe(0)
  })

  it('后端新增而前端还没跟上的 provider 不被吞掉，排在已知项之后', () => {
    const tabs = buildProviderTabs([pRow('x1', 'zeta'), pRow('y1', 'acme'), pRow('k1', 'kiro')])
    expect(tabs.map((t) => t.provider)).toEqual([
      'kiro',
      'claude-dario',
      'cursor',
      'acme',
      'zeta',
    ])
    expect(tabs.find((t) => t.provider === 'zeta')?.count).toBe(1)
  })
})

describe('模型不可用标记剩余时间格式化', () => {
  it('< 1h 显示分钟，下限 1m（0s/负值不显示 0m）', () => {
    expect(formatMarkTtl(1800)).toBe('30m')
    expect(formatMarkTtl(59 * 60)).toBe('59m')
    expect(formatMarkTtl(0)).toBe('1m')
  })

  it('>= 1h 显示小时：整点不带小数，非整点 1 位小数', () => {
    expect(formatMarkTtl(3600)).toBe('1h')
    expect(formatMarkTtl(6 * 3600)).toBe('6h')
    expect(formatMarkTtl(5400)).toBe('1.5h')
  })
})

/** 最小模型条目:只填三态推导用得到的字段。 */
function modelEntry(supported: boolean, mark: number | null): AccountModelEntry {
  return {
    id: 'claude-opus-5',
    display_name: null,
    supported,
    mark_remaining_secs: mark,
    available: supported && mark === null,
  }
}

describe('模型可用三态推导（查看模型弹窗）', () => {
  it('支持且无标记 → 可用', () => {
    expect(deriveModelAvailability(modelEntry(true, null))).toBe('available')
  })

  it('支持但有标记 → 被标记（与 available=false 一致）', () => {
    expect(deriveModelAvailability(modelEntry(true, 1200))).toBe('marked')
  })

  it('标记剩余 0s（即将到期）仍算标记中，与后端 is_none() 判定对齐', () => {
    expect(deriveModelAvailability(modelEntry(true, 0))).toBe('marked')
  })

  it('档位不支持优先于标记：两种不可用必须分开展示', () => {
    expect(deriveModelAvailability(modelEntry(false, null))).toBe('unsupported')
    expect(deriveModelAvailability(modelEntry(false, 300))).toBe('unsupported')
  })
})

/** 最小运行态条目:只填状态推导用得到的字段。 */
function rt(reason: AccountUnavailableReason, online = true): AccountRuntimeEntry {
  return {
    online,
    status: {
      account_id: 'a',
      priority: 0,
      disabled: reason !== '',
      reason,
      cooldown_remaining_secs: 0,
      failure_count: 0,
      available_permits: 1,
      max_concurrency: 1,
    },
  }
}

describe('状态推导（配置 × 运行态 merge）', () => {
  it('配置停用 + runtime 报退役 → 显示「已退役」而不是普通「已停用」', () => {
    // 自动退役会落库 disabled=1;先判 row.disabled 会把退役永远遮成「已停用」(审查 [中])。
    const r = { ...row('a', 1), disabled: true }
    expect(deriveAccountStatus(r, rt('suspended_retired'))).toEqual({ kind: 'retired' })
  })

  it('配置停用但 runtime 无退役原因 → 仍是普通「已停用」(人工停用不冒名退役)', () => {
    const r = { ...row('a', 1), disabled: true }
    expect(deriveAccountStatus(r, rt(''))).toEqual({ kind: 'disabled' })
    expect(deriveAccountStatus(r, rt('quota_exhausted'))).toEqual({ kind: 'disabled' })
    // runtime 离线/缺失时不妄断,按配置展示。
    expect(deriveAccountStatus(r, rt('suspended_retired', false))).toEqual({ kind: 'disabled' })
    expect(deriveAccountStatus(r, undefined)).toEqual({ kind: 'disabled' })
  })

  it('runtime 退役/观察期归入异常分桶,不进「已停用」桶', () => {
    expect(accountStatusBucket({ kind: 'retired' })).toBe('abnormal')
    expect(accountStatusBucket({ kind: 'probation' })).toBe('abnormal')
    expect(accountStatusBucket({ kind: 'disabled' })).toBe('disabled')
  })

  it('runtime 直接报退役(配置未停用) → 退役', () => {
    expect(deriveAccountStatus(row('a', 1), rt('suspended_retired'))).toEqual({ kind: 'retired' })
  })
})

describe('Cursor 官方登录轮询状态机', () => {
  it('pending 且未超时 → 继续轮询', () => {
    expect(decideCursorLoginPoll({ kind: 'pending' }, false)).toBe('continue')
  })

  it('done → 成功（即使刚好越过截止线也算成功，凭据已落库）', () => {
    expect(decideCursorLoginPoll({ kind: 'done' }, false)).toBe('success')
    expect(decideCursorLoginPoll({ kind: 'done' }, true)).toBe('success')
  })

  it('超过 expires_in_sec 窗口 → 超时停止（会话已被后端清扫）', () => {
    expect(decideCursorLoginPoll({ kind: 'pending' }, true)).toBe('timeout')
    expect(decideCursorLoginPoll({ kind: 'error', status: 502 }, true)).toBe('timeout')
  })

  it('502（后端到上游的瞬时网络错误）→ 继续轮询，会话还在', () => {
    expect(decideCursorLoginPoll({ kind: 'error', status: 502 }, false)).toBe('continue')
  })

  it('传输层错误（无 HTTP 响应）→ 同样按瞬时处理，继续轮询', () => {
    expect(decideCursorLoginPoll({ kind: 'error', status: undefined }, false)).toBe('continue')
  })

  it('4xx（会话已清的终态失败）→ 停止轮询并报错', () => {
    expect(decideCursorLoginPoll({ kind: 'error', status: 400 }, false)).toBe('fail')
    expect(decideCursorLoginPoll({ kind: 'error', status: 401 }, false)).toBe('fail')
    expect(decideCursorLoginPoll({ kind: 'error', status: 409 }, false)).toBe('fail')
  })

  it('未约定的其他 5xx → 保守按终态失败停止', () => {
    expect(decideCursorLoginPoll({ kind: 'error', status: 500 }, false)).toBe('fail')
    expect(decideCursorLoginPoll({ kind: 'error', status: 503 }, false)).toBe('fail')
  })
})

describe('超额（on-demand）额度展示', () => {
  const od = (p: Partial<OnDemandQuota>): OnDemandQuota => ({
    enabled: true,
    used: 0,
    unlimited: false,
    ...p,
  })

  it('未开启 → off（面板显示「关」，不是 —）', () => {
    expect(onDemandState(od({ enabled: false }))).toBe('off')
  })

  it('已开启 + 有上限 → on', () => {
    expect(onDemandState(od({ limit: 75 }))).toBe('on')
  })

  it('已开启但不限额 → unlimited（上游 i32::MAX 已在后端归一）', () => {
    expect(onDemandState(od({ unlimited: true }))).toBe('unlimited')
  })

  it('已开启但上限缺省/为 0 → 按不限额展示，不显示 $0 上限', () => {
    expect(onDemandState(od({ limit: null }))).toBe('unlimited')
    expect(onDemandState(od({ limit: 0 }))).toBe('unlimited')
  })

  it('已用达上限 80% 起标黄（触顶后上游会拒请求）', () => {
    expect(isOnDemandHigh(od({ limit: 100, used: 79 }))).toBe(false)
    expect(isOnDemandHigh(od({ limit: 100, used: 80 }))).toBe(true)
    expect(isOnDemandHigh(od({ limit: 100, used: 120 }))).toBe(true)
  })

  it('未开启/不限额永不标黄（否则一堆号常驻黄字）', () => {
    expect(isOnDemandHigh(od({ enabled: false, used: 999 }))).toBe(false)
    expect(isOnDemandHigh(od({ unlimited: true, used: 999 }))).toBe(false)
  })

  it('美元格式：整数不带小数，小数保留两位', () => {
    expect(formatUsd(75)).toBe('$75')
    expect(formatUsd(22.5)).toBe('$22.50')
    expect(formatUsd(0)).toBe('$0')
  })
})
