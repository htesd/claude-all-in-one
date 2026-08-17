import { useEffect, useState, type FormEvent } from 'react'
import { ChevronRight, Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Segment } from '@/components/ui/segment'
import { Select } from '@/components/ui/select'
import { useGroups, useSaveMemberships } from '@/features/groups/hooks'
import { extractErrorMessage } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { useUpdateAccount } from '../hooks'
import {
  buildRotatedExtra,
  diffMemberships,
  getMaskedRefreshToken,
  LOW_PRIORITY,
  parseConcurrency,
  priorityToTier,
  tierToPriority,
  type PriorityTier,
} from '../lib'
import type { AccountGroupMembership, AccountRow, UpdateAccountPayload } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm outline-none transition-colors placeholder:text-muted-foreground'

interface EditAccountDialogProps {
  open: boolean
  /** 被编辑的账号行；open 为 true 时必有值。 */
  row: AccountRow | null
  onClose: () => void
}

/**
 * 编辑账号对话框：归属分组、**成员分组（每组独立优先级）**、并发数，
 * 以及可选的「更换凭据」折叠区。
 *
 * 归属（`group_name`）与成员边（`account_groups`）是两件事：前者只决定哪个 worker
 * 独占管它的运行态，后者才决定谁能用到它、在那个组里排第几。调度读的是**后者**。
 *
 * 凭据 textarea 留空 = 不动 extra；填了 = 整体替换（保留原非敏感字段，
 * 脱敏的 `***` 字段绝不回写）。
 */
export function EditAccountDialog({ open, row, onClose }: EditAccountDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const mutation = useUpdateAccount()
  const saveMemberships = useSaveMemberships()

  const [group, setGroup] = useState('')
  const [concurrency, setConcurrency] = useState('1')
  /**
   * 勾选的成员分组 → 组内优先级**原始数值**。键不存在 = 没勾。
   *
   * 存数值而不是高/低两档:后端 priority 是任意 i64,而 UI 只暴露两档。若草稿存档位、
   * 提交时再映射回 0/100,一条实际优先级 50 的边会在运维只改并发时被静默改写成 0。
   * 存原值 → 没碰过的组数值原样,差集判定"没变",一个请求都不发。
   */
  const [memberships, setMemberships] = useState<Record<string, number>>({})
  const [rotateOpen, setRotateOpen] = useState(false)
  const [token, setToken] = useState('')
  const [queueEnabled, setQueueEnabled] = useState(false)
  const [proxyUrl, setProxyUrl] = useState('')
  const [initialProxyUrl, setInitialProxyUrl] = useState('')
  // 模型白名单(extra.model_allowlist):UI 层是逗号分隔串;后端写侧校验并
  // 规范化成 JSON 数组落库。空串 = 清除(不限)。
  const [modelAllowlist, setModelAllowlist] = useState('')
  const [initialModelAllowlist, setInitialModelAllowlist] = useState('')
  // 上游驱动形态(extra.driver,cursor 专用)。2026-08-17 起 **CLI 驱动是默认**:
  // '' = 默认(CLI 子进程驱动 cursor-agent),'wire' = 该号退回线协议。
  // 历史值 'cli' 与 '' 等价(都是默认),所以回填时一并归一成 ''。
  const [driver, setDriver] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  // 打开时预填当前值；编辑过程中列表 refetch 换引用不打断草稿
  useEffect(() => {
    if (open && row) {
      setGroup(row.group_name)
      setConcurrency(String(row.max_concurrency))
      setMemberships(Object.fromEntries((row.groups ?? []).map((m) => [m.name, m.priority])))
      setQueueEnabled(row.queue_enabled ?? false)
      setRotateOpen(false)
      setToken('')
      const currentProxy = typeof row.extra.proxy === 'string' ? row.extra.proxy : ''
      setProxyUrl(currentProxy)
      setInitialProxyUrl(currentProxy)
      // 顶层回显是规范化后的 JSON 数组(或 null=不限);UI 层展示成逗号串。
      const currentAllowlist = Array.isArray(row.model_allowlist)
        ? row.model_allowlist.join(', ')
        : ''
      setModelAllowlist(currentAllowlist)
      setInitialModelAllowlist(currentAllowlist)
      setDriver(row.driver === 'wire' ? 'wire' : '')
      setError(null)
      setSaving(false)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const maskedToken = row ? getMaskedRefreshToken(row.extra) : null
  const busy = saving || mutation.isPending || saveMemberships.isPending

  const toggleMembership = (name: string) => {
    setMemberships((prev) => {
      if (name in prev) {
        const { [name]: _removed, ...rest } = prev
        return rest
      }
      // 新勾选默认「低」——和后端 default_member_priority 一致，也是更安全的一档：
      // 新号直接进高优先层会一上来吃掉全部新会话（2026-07-29 五个号 22 分钟被封的配置）。
      return { ...prev, [name]: LOW_PRIORITY }
    })
  }

  /** 只有用户**真的点了**高/低才写 0/100；没点过的组保持库里的原始数值。 */
  const setMembershipTier = (name: string, tier: PriorityTier) => {
    setMemberships((prev) => (name in prev ? { ...prev, [name]: tierToPriority(tier) } : prev))
  }

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (!row || busy) return

    const parsedConcurrency = parseConcurrency(concurrency)
    if (parsedConcurrency === null) {
      setError(t('accounts.error.invalidConcurrency'))
      return
    }
    setError(null)

    // 只 PATCH 变化的字段；凭据留空 = 不传 extra
    const patch: UpdateAccountPayload = {}
    if (group !== row.group_name) patch.group_name = group
    if (parsedConcurrency !== row.max_concurrency) patch.max_concurrency = parsedConcurrency
    const trimmedToken = token.trim()
    if (trimmedToken !== '') patch.extra = buildRotatedExtra(row.extra, trimmedToken)
    // proxy_url: 不传=不动，'' = 清除，非空字符串 = 设置
    if (proxyUrl !== initialProxyUrl) patch.proxy_url = proxyUrl
    // 排队开关：与原值比较，没动就不带 —— 否则对着不认这个字段的旧后端，
    // 只改并发也会让**整个保存**被判 400。
    if (queueEnabled !== (row.queue_enabled ?? false)) patch.queue_enabled = queueEnabled
    // model_allowlist: 不传=不动，'' = 清除（不限），非空 = 逗号分隔条目
    //（后端写侧校验：通配符只许末尾、非法字符 400，规范化成 JSON 数组落库）。
    if (modelAllowlist !== initialModelAllowlist) patch.model_allowlist = modelAllowlist
    // 驱动形态：与原值比较，没动就不带（同 queue_enabled 的理由：对着不认这个字段的
    // 旧后端，只改并发也会让整个保存被判 400）。'' = 清除回线协议。
    if (driver !== (row.driver === 'wire' ? 'wire' : '')) patch.driver = driver

    const draft: AccountGroupMembership[] = Object.entries(memberships).map(
      ([name, priority]) => ({ name, priority }),
    )
    const { upserts, removals } = diffMemberships(row.groups ?? [], draft)

    if (Object.keys(patch).length === 0 && upserts.length === 0 && removals.length === 0) {
      onClose()
      return
    }

    void (async () => {
      setSaving(true)
      try {
        // 账号字段先落：它失败(如改归属触发 CrossOwner 400)就整单中止，
        // 一条成员边都不动，不留半成品。
        if (Object.keys(patch).length > 0) {
          await mutation.mutateAsync({ id: row.account_id, patch })
        }
        const { failures } = await saveMemberships.mutateAsync({
          accountId: row.account_id,
          upserts,
          removals,
        })
        if (failures.length > 0) {
          // 不关弹窗。mutation 的 onSettled 已经失效了账号查询，重新拉到的 row.groups
          // 就是新基线，用户改掉出错的那几个再提交时差集自动跳过已成功的部分。
          setError(`${t('accounts.memberships.failed')} ${failures.join('；')}`)
          return
        }
        onClose()
      } catch (err) {
        setError(extractErrorMessage(err))
      } finally {
        setSaving(false)
      }
    })()
  }

  return (
    <Modal open={open && row !== null} onClose={onClose} title={t('accounts.edit.title')}>
      {row !== null && (
        <form onSubmit={handleSubmit} className="mt-4 space-y-4">
          {/* 账号 ID（只读） */}
          <div className="space-y-1.5">
            <span className="text-xs font-medium text-muted-foreground">
              {t('accounts.field.id')}
            </span>
            <p>
              <code className="rounded-md bg-muted px-2 py-0.5 font-mono text-xs">
                {row.account_id}
              </code>
            </p>
          </div>

          {/* 归属分组（owner）——只管运行态归哪个 worker，不是权限 */}
          <div className="space-y-1.5">
            <label
              htmlFor="edit-account-group"
              className="text-xs font-medium text-muted-foreground"
            >
              {t('accounts.field.owner')}
            </label>
            <Select
              id="edit-account-group"
              value={group}
              onChange={(event) => setGroup(event.target.value)}
              className="w-full"
            >
              <option value="">{t('groups.ungrouped')}</option>
              {(groupsQuery.data ?? []).map((g) => (
                <option key={g.name} value={g.name}>
                  {g.name}
                </option>
              ))}
            </Select>
            <p className="text-xs text-muted-foreground">{t('accounts.field.ownerHint')}</p>
          </div>

          {/* 成员分组：谁能用这个号 + 组内排第几（调度真正读的就是这里） */}
          <div className="space-y-1.5">
            <span className="text-xs font-medium text-muted-foreground">
              {t('accounts.field.memberships')}
            </span>
            <div className="space-y-1 rounded-2xl border p-2">
              {(groupsQuery.data ?? []).map((g) => {
                const priority = memberships[g.name]
                const checked = priority !== undefined
                const tier = checked ? priorityToTier(priority) : undefined
                return (
                  <div key={g.name} className="flex items-center gap-2 px-1 py-0.5">
                    <label className="flex flex-1 items-center gap-2 text-sm">
                      <input
                        type="checkbox"
                        checked={checked}
                        onChange={() => toggleMembership(g.name)}
                        className="h-4 w-4 rounded border accent-primary"
                      />
                      <span className={cn(!checked && 'text-muted-foreground')}>{g.name}</span>
                    </label>
                    <div
                      className={cn(
                        'inline-flex rounded-xl bg-muted p-0.5 text-xs font-medium transition-opacity',
                        !checked && 'pointer-events-none opacity-40',
                      )}
                    >
                      {(['high', 'low'] as const).map((value) => (
                        <button
                          key={value}
                          type="button"
                          disabled={!checked}
                          onClick={() => setMembershipTier(g.name, value)}
                          className={cn(
                            'rounded-lg px-2.5 py-1 transition-colors',
                            tier === value
                              ? 'bg-background shadow-sm'
                              : 'text-muted-foreground',
                          )}
                        >
                          {t(`accounts.priorityTier.${value}`)}
                        </button>
                      ))}
                    </div>
                  </div>
                )
              })}
            </div>
            <p className="text-xs text-muted-foreground">{t('accounts.field.membershipsHint')}</p>
            {Object.keys(memberships).length === 0 && (
              <p className="text-xs text-destructive">{t('accounts.memberships.empty')}</p>
            )}
          </div>

          {/* 并发上限 */}
          <div className="space-y-1.5">
            <label
              htmlFor="edit-account-concurrency"
              className="text-xs font-medium text-muted-foreground"
            >
              {t('accounts.field.concurrency')}
            </label>
            <input
              id="edit-account-concurrency"
              type="number"
              min={1}
              step={1}
              value={concurrency}
              onChange={(event) => setConcurrency(event.target.value)}
              className={inputClass}
            />
          </div>

          {/* 排队等冷却：逐号开关。
              企业号（ksk_/IdC）的上游并发跨租户共享，429 是跟别的买家抢同一个池子，
              等一下真的就有 —— 开了以后客户感知不到限速，只是变慢。
              社交号的 429 常伴额度见底，等待只会把客户多挂几秒后照样报错，别开。 */}
          <div className="space-y-1.5">
            <span className="text-xs font-medium text-muted-foreground">
              {t('accounts.field.queue')}
            </span>
            <div className="flex flex-wrap items-center gap-2">
              <Segment
                options={[
                  { value: 'off', label: t('accounts.queue.off') },
                  { value: 'on', label: t('accounts.queue.on') },
                ]}
                value={queueEnabled ? 'on' : 'off'}
                onChange={(v) => setQueueEnabled(v === 'on')}
              />
            </div>
            <p className="text-xs text-muted-foreground">{t('accounts.field.queueHint')}</p>
          </div>

          {/* 上游驱动形态：只对 cursor 家族有意义（别的 provider 读不到 extra.driver，
              露出来只会误导）。线协议 = 自己拼 protobuf 打 agent.v1.AgentService/Run；
              CLI = 子进程驱动官方 cursor-agent，usage 是上游真实值（含 cacheRead），
              工具走 MCP 桥回路。切换约 30s 内经 worker sync 生效，不用重启。 */}
          {row?.provider === 'cursor' && (
            <div className="space-y-1.5">
              <span className="text-xs font-medium text-muted-foreground">
                {t('accounts.field.driver')}
              </span>
              <div className="flex flex-wrap items-center gap-2">
                <Segment
                  options={[
                    { value: '', label: t('accounts.driver.cli') },
                    { value: 'wire', label: t('accounts.driver.wire') },
                  ]}
                  value={driver}
                  onChange={(v) => setDriver(v)}
                />
              </div>
              <p className="text-xs text-muted-foreground">{t('accounts.field.driverHint')}</p>
            </div>
          )}

          {/* 出口代理（可选） */}
          <div className="space-y-1.5">
            <label
              htmlFor="edit-account-proxy"
              className="text-xs font-medium text-muted-foreground"
            >
              {t('accounts.field.proxy')}
            </label>
            <input
              id="edit-account-proxy"
              type="text"
              value={proxyUrl}
              onChange={(event) => setProxyUrl(event.target.value)}
              placeholder={t('accounts.field.proxyPlaceholder')}
              spellCheck={false}
              autoComplete="off"
              className={inputClass}
            />
          </div>

          {/* 模型白名单（可选）：该号允许服务的模型，逗号分隔；留空 = 不限。
              条目是 Run 侧模型名或「前缀*」（星号仅限末尾），后端写侧校验、
              规范化成 JSON 数组落库。典型：只有自家模型的号写 default,composer*,grok* */}
          <div className="space-y-1.5">
            <label
              htmlFor="edit-account-model-allowlist"
              className="text-xs font-medium text-muted-foreground"
            >
              {t('accounts.field.modelAllowlist')}
            </label>
            <input
              id="edit-account-model-allowlist"
              type="text"
              value={modelAllowlist}
              onChange={(event) => setModelAllowlist(event.target.value)}
              placeholder={t('accounts.field.modelAllowlistPlaceholder')}
              spellCheck={false}
              autoComplete="off"
              className={inputClass}
            />
            <p className="text-xs text-muted-foreground">
              {t('accounts.field.modelAllowlistHint')}
            </p>
          </div>

          {/* 更换凭据：默认折叠；展示脱敏尾号便于核对 */}
          <div className="space-y-1.5">
            <button
              type="button"
              onClick={() => setRotateOpen((prev) => !prev)}
              className="inline-flex items-center gap-1 rounded text-xs font-medium text-muted-foreground transition-colors hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
            >
              <ChevronRight
                className={cn('h-3.5 w-3.5 transition-transform', rotateOpen && 'rotate-90')}
              />
              {t('accounts.edit.rotateToggle')}
            </button>
            {rotateOpen && (
              <div className="space-y-1.5">
                <p className="text-xs text-muted-foreground">
                  {t('accounts.edit.currentToken')}:{' '}
                  <code className="rounded bg-muted px-1.5 py-0.5 font-mono">
                    {maskedToken ?? '—'}
                  </code>
                </p>
                <textarea
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                  placeholder={t('accounts.field.refreshTokenPlaceholder')}
                  rows={3}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} resize-none font-mono text-xs leading-5`}
                />
                <p className="text-xs text-muted-foreground">{t('accounts.edit.rotateHint')}</p>
              </div>
            )}
          </div>

          {error !== null && <p className="text-sm text-destructive">{error}</p>}

          {/* worker 周期同步提示 */}
          <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Info className="h-3.5 w-3.5 shrink-0" />
            {t('accounts.syncHint')}
          </p>

          <div className="flex justify-end gap-2 pt-1">
            <Button variant="ghost" onClick={onClose}>
              {t('common.cancel')}
            </Button>
            <Button type="submit" disabled={busy}>
              {busy && <Loader2 className="h-4 w-4 animate-spin" />}
              {busy ? t('accounts.edit.saving') : t('accounts.edit.submit')}
            </Button>
          </div>
        </form>
      )}
    </Modal>
  )
}
