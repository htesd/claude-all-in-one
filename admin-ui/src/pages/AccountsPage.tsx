import { useEffect, useMemo, useState } from 'react'
import { AlertTriangle, CheckCircle2, KeyRound, MousePointer2, Upload, Users } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { ErrorNote } from '@/components/ui/error-note'
import { cn } from '@/lib/utils'
import { AccountsTable } from '@/features/accounts/components/AccountsTable'
import type { RuntimeQueryState } from '@/features/accounts/components/AccountTableRow'
import { AccountModelsDialog } from '@/features/accounts/components/AccountModelsDialog'
import { OnDemandDialog } from '@/features/accounts/components/OnDemandDialog'
import {
  AccountsFilterBar,
  type StatusFilter,
  type TierFilter,
} from '@/features/accounts/components/AccountsFilterBar'
import {
  AccountsPagination,
  type PageSize,
} from '@/features/accounts/components/AccountsPagination'
import { CursorAccountDialog } from '@/features/accounts/components/CursorAccountDialog'
import { EditAccountDialog } from '@/features/accounts/components/EditAccountDialog'
import { ImportAccountsDialog } from '@/features/accounts/components/ImportAccountsDialog'
import { OAuthAccountDialog } from '@/features/accounts/components/OAuthAccountDialog'
import {
  useAccounts,
  useAccountsRuntime,
  useDeleteAccount,
  useRefreshAccount,
  useResetAccount,
  useUpdateAccount,
} from '@/features/accounts/hooks'
import type { RefreshAccountResult } from '@/features/accounts/api'
import {
  accountStatusBucket,
  buildProviderTabs,
  deriveAccountStatus,
  deriveTier,
  mergeRuntimeByAccount,
  quotaKindForProvider,
  sortAccounts,
  type AccountSortKey,
} from '@/features/accounts/lib'
import type { AccountRow } from '@/features/accounts/types'
import { useGroups } from '@/features/groups/hooks'
import { useI18n } from '@/lib/i18n'

export default function AccountsPage() {
  const { t } = useI18n()

  const accountsQuery = useAccounts()
  // worker 运行态 15s 轮询；失败不致命（状态列降级展示）
  const runtimeQuery = useAccountsRuntime()
  const groupsQuery = useGroups()

  const updateMutation = useUpdateAccount()
  const deleteMutation = useDeleteAccount()
  const resetMutation = useResetAccount()
  const refreshMutation = useRefreshAccount()

  const [importOpen, setImportOpen] = useState(false)
  const [oauthOpen, setOauthOpen] = useState(false)
  const [cursorOpen, setCursorOpen] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null)
  // 「查看模型」弹窗目标（仅 kiro 行有入口；纯本地查询,无需 mutation/反馈条）。
  const [modelsAccountId, setModelsAccountId] = useState<string | null>(null)
  // 超额设置弹窗的目标账号 id（跟随列表数据取行，行被删则自动关闭）。
  const [onDemandId, setOnDemandId] = useState<string | null>(null)
  // 刷新 token 成功的轻量反馈（无 toast 库）：账号 + 新有效期。本地态而非派生自
  // refreshMutation.isSuccess——后者会在其它 mutation 成功后残留旧账号（审查 3 名 reviewer）。
  // 任一操作（含再次刷新）开始即清空，做到注释承诺的"下次操作自动消失"。
  const [refreshOk, setRefreshOk] = useState<RefreshAccountResult | null>(null)

  // 筛选状态
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all')
  const [providerFilter, setProviderFilter] = useState<string>('all')
  const [tierFilter, setTierFilter] = useState<TierFilter>('all')

  // 排序状态。默认「最新在前」:Kiro 短命号(ksk_ 寿命二三十分钟、成批死亡)只能按
  // 上号时间管理,而后端固定返回「按组+账号名」——那个顺序里新号会散落在各处。
  const [sortKey, setSortKey] = useState<AccountSortKey>('created_desc')

  // 分页状态
  const [page, setPage] = useState(1)
  const [pageSize, setPageSize] = useState<PageSize>(20)

  const groupColors = useMemo(() => {
    const map = new Map<string, string>()
    for (const group of groupsQuery.data ?? []) map.set(group.name, group.color)
    return map
  }, [groupsQuery.data])

  const runtimeByAccount = useMemo(
    () => mergeRuntimeByAccount(runtimeQuery.data),
    [runtimeQuery.data],
  )

  // provider 筛选项：已知 provider 常驻（计数 0 也列），kiro→ccmax→cursor→未知
  const providers = useMemo(() => buildProviderTabs(accountsQuery.data), [accountsQuery.data])

  // 检查是否有 kiro 账号（决定是否展示档位筛选）
  const hasKiroAccounts = useMemo(
    () => (accountsQuery.data ?? []).some((row) => row.provider === 'kiro'),
    [accountsQuery.data],
  )

  // 在 kiro/all 且有 kiro 账号时展示档位筛选
  const showTierFilter =
    hasKiroAccounts && (providerFilter === 'kiro' || providerFilter === 'all')

  // 数据中出现过的档位（去重有序）
  const distinctTiers = useMemo((): TierFilter[] => {
    const set = new Set<TierFilter>()
    for (const row of accountsQuery.data ?? []) {
      if (row.provider !== 'kiro') continue
      const tier = deriveTier(row, runtimeByAccount.get(row.account_id))
      if (tier !== null) set.add(tier)
    }
    // 有序：PRO / POWER / FREE / OTHER
    const order: TierFilter[] = ['PRO', 'POWER', 'FREE', 'OTHER']
    return order.filter((t) => set.has(t))
  }, [accountsQuery.data, runtimeByAccount])

  // 当 provider filter 变动后，如果当前 tierFilter 在新 provider 下不再可见，重置
  useEffect(() => {
    if (!showTierFilter) setTierFilter('all')
  }, [showTierFilter])

  // 筛选后行 → 排序 → 分页行
  const filteredRows = useMemo(() => {
    const all = accountsQuery.data ?? []
    const kept = all.filter((row) => {
      // provider 筛选
      if (providerFilter !== 'all' && row.provider !== providerFilter) return false

      // 档位筛选（只对 kiro 账号生效）
      if (tierFilter !== 'all' && row.provider === 'kiro') {
        const tier = deriveTier(row, runtimeByAccount.get(row.account_id))
        if (tier !== tierFilter) return false
      }

      // 状态筛选
      if (statusFilter !== 'all') {
        const runtime = runtimeByAccount.get(row.account_id)
        const displayStatus = deriveAccountStatus(row, runtime)
        const bucket = accountStatusBucket(displayStatus)
        if (bucket !== statusFilter) return false
      }

      return true
    })
    // 排序放在筛选之后、分页之前:先筛后排才能让「第 1 页」始终是当前筛选下最新的号。
    return sortAccounts(kept, sortKey)
  }, [accountsQuery.data, providerFilter, tierFilter, statusFilter, runtimeByAccount, sortKey])

  // 任一筛选/排序变化时重置到第 1 页
  useEffect(() => {
    setPage(1)
  }, [statusFilter, providerFilter, tierFilter, sortKey, pageSize])

  // 页码在渲染期钳到有效范围(不写回 state):筛选变化或 15s 运行态轮询令结果集缩小时,
  // 避免停留在越界的空白页(审查 H1/M1)。故意不走 setPage——否则每次运行态轮询都会把
  // 正在翻页的用户拽回第 1 页。分页切片与分页控件都用这个 currentPage,口径一致。
  const totalPages = Math.max(1, Math.ceil(filteredRows.length / pageSize))
  const currentPage = Math.min(page, totalPages)

  // 分页切片
  const pagedRows = useMemo(() => {
    const start = (currentPage - 1) * pageSize
    return filteredRows.slice(start, start + pageSize)
  }, [filteredRows, currentPage, pageSize])

  // 当前 provider 的配额口径（按 providerFilter 决定；'all' 时回落第一个**有号**的 provider）。
  // 注意必须 count > 0：providers 现在含计数为 0 的常驻项，取 providers[0] 会恒为 kiro,
  // 于是一台只有 cursor 号的机器配额表头会误称「积分」（cursor 是订阅制、无配额数字）。
  const effectiveProviderForQuota =
    providerFilter !== 'all'
      ? providerFilter
      : (providers.find((p) => p.count > 0)?.provider ?? '')
  const quotaKind = quotaKindForProvider(effectiveProviderForQuota)
  // 超额列:**当前页里有 cursor 号就显示**,而不是看 effectiveProviderForQuota。
  // 后者在默认的 'all' 视图下会回落成「第一个有号的 provider」(混部机器上通常是
  // kiro),于是超额列默认整列不渲染 —— 运维必须先把筛选切到 Cursor 才看得见,
  // 而「每个 cursor 号超额多少」恰恰是要一眼可见的信息。
  // 非 cursor 行在该列显示「—」(见 AccountTableRow:provider 不支持则无 on_demand),
  // 这点噪音远小于默认看不见的代价。
  const showOnDemand = pagedRows.some((r) => r.provider === 'cursor')

  // 轮询失败但还有旧数据时继续按旧数据展示（配合下方警示条）；完全没数据才降级
  const runtimeState: RuntimeQueryState = runtimeQuery.isPending
    ? 'loading'
    : runtimeQuery.isError && runtimeQuery.data === undefined
      ? 'error'
      : 'ready'

  // 编辑对话框跟随列表数据（refetch 后行被删则自动关闭）
  const editingRow =
    editingId !== null
      ? (accountsQuery.data?.find((row) => row.account_id === editingId) ?? null)
      : null

  // 「查看模型」弹窗同理跟随列表数据：需要整行（provider 决定可否编辑白名单、
  // model_allowlist 决定勾选基线），行被删则自动关闭。
  const modelsRow =
    modelsAccountId !== null
      ? (accountsQuery.data?.find((row) => row.account_id === modelsAccountId) ?? null)
      : null

  // 超额弹窗同理跟随列表数据；当前快照取 runtime 里的配额缓存（与表格同一数据源，
  // 设置成功后 invalidate 会一并刷新，弹窗里的「当前」不会停在旧值）。
  const onDemandRow =
    onDemandId !== null
      ? (accountsQuery.data?.find((row) => row.account_id === onDemandId) ?? null)
      : null
  const onDemandRuntime = onDemandId !== null ? runtimeByAccount.get(onDemandId) : undefined
  const onDemandCurrent = onDemandRuntime?.online
    ? onDemandRuntime.status.quota?.on_demand
    : undefined

  // 当前有 mutation 进行中的 account_id —— 只置灰对应行的按钮
  const busyId = updateMutation.isPending
    ? (updateMutation.variables?.id ?? null)
    : deleteMutation.isPending
      ? (deleteMutation.variables ?? null)
      : resetMutation.isPending
        ? (resetMutation.variables ?? null)
        : refreshMutation.isPending
          ? (refreshMutation.variables ?? null)
          : null

  // 每个操作发起即清掉上一次的刷新成功提示（"下次操作自动消失"）。
  const handleToggleDisabled = (row: AccountRow) => {
    setRefreshOk(null)
    updateMutation.mutate({ id: row.account_id, patch: { disabled: !row.disabled } })
  }
  const handleDelete = (id: string) => {
    setRefreshOk(null)
    deleteMutation.mutate(id)
  }
  const handleReset = (id: string) => {
    setRefreshOk(null)
    resetMutation.mutate(id)
  }
  const handleRefresh = (id: string) => {
    setRefreshOk(null)
    refreshMutation.mutate(id, { onSuccess: (data) => setRefreshOk(data) })
  }

  const actionError = updateMutation.isError
    ? updateMutation.error
    : deleteMutation.isError
      ? deleteMutation.error
      : resetMutation.isError
        ? resetMutation.error
        : refreshMutation.isError
          ? refreshMutation.error
          : null

  return (
    <div className="space-y-6">
      {/* Page header */}
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <p className="eyebrow">Accounts</p>
          <h1 className="mt-2 font-display text-4xl font-black tracking-[-0.04em]">{t('accounts.title')}</h1>
          <p className="mt-2 text-sm text-muted-foreground">{t('accounts.subtitle')}</p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="outline" onClick={() => setImportOpen(true)}>
            <Upload className="h-4 w-4" />
            {t('accounts.import')}
          </Button>
          <Button variant="outline" onClick={() => setOauthOpen(true)}>
            <KeyRound className="h-4 w-4" />
            {t('accounts.oauth')}
          </Button>
          <Button variant="outline" onClick={() => setCursorOpen(true)}>
            <MousePointer2 className="h-4 w-4" />
            {t('accounts.cursor')}
          </Button>
        </div>
      </header>

      {accountsQuery.isError && <ErrorNote error={accountsQuery.error} />}
      {/* 启停/删除/刷新失败时的提示（下一次操作发起时自动清除） */}
      {actionError !== null && <ErrorNote error={actionError} labelKey="common.actionFailed" />}

      {/* 刷新 token 成功的轻量反馈：账号 + 新 token 有效期（下次操作自动消失）。
          有 actionError 时不显示，避免成功条与残留错误条同时出现自相矛盾。 */}
      {refreshOk !== null && actionError === null && (
        <div className="flex flex-wrap items-center gap-1.5 px-1 text-xs text-success">
          <CheckCircle2 className="h-3.5 w-3.5 shrink-0" />
          <span>{t('accounts.refresh.success')}</span>
          <code className="rounded bg-muted px-1 font-mono">{refreshOk.account_id}</code>
          {refreshOk.expires_at && (
            <span className="text-muted-foreground">
              {t('accounts.refresh.expiresAt')} {new Date(refreshOk.expires_at).toLocaleString()}
            </span>
          )}
        </div>
      )}

      {/* 排队实况：waiting/capacity。容量只算**开了排队且当前可服务**的号的并发之和，
          所以这个比值是真实拥挤度 —— 不会因为库里躺着一堆额度跑干的号而虚高。
          waiting 触到 capacity 时新请求立刻 503（不再排进来陪跑到超时）。
          旧 worker 不返回 queue 字段 → 整块不渲染，而不是显示 0/0 误导。 */}
      {(() => {
        const instances = (runtimeQuery.data ?? []).filter((i) => i.online && i.queue)
        if (instances.length === 0) return null
        return (
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 px-1 text-xs">
            {instances.map((i) => {
              const q = i.queue!
              const full = q.capacity > 0 && q.waiting >= q.capacity
              return (
                <span key={i.instance} className="flex items-center gap-1.5">
                  <Users className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                  <span className="text-muted-foreground">
                    {t('accounts.queue.title')}
                    {instances.length > 1 && ` · ${i.group}`}
                  </span>
                  <span
                    className={cn('tabular-nums font-medium', full && 'text-destructive')}
                    title={t('accounts.queue.statHint')}
                  >
                    {q.waiting} / {q.capacity}
                  </span>
                  <span className="text-muted-foreground">
                    ({t('accounts.queue.enabledAccounts', { n: q.enabled_accounts })})
                  </span>
                  {/* 累计值:waiting 几乎恒为 0,只有累计数能说明机制有没有真的在工作。 */}
                  <span className="text-muted-foreground" title={t('accounts.queue.totalsHint')}>
                    {t('accounts.queue.totals', {
                      queued: q.queued_total ?? 0,
                      paced: q.paced_total ?? 0,
                    })}
                  </span>
                </span>
              )
            })}
          </div>
        )
      })()}

      {/* runtime 失败不致命：账号列表照常展示，但要明示状态列不可信 */}
      {runtimeQuery.isError && (
        <div className="flex items-center gap-1.5 px-1 text-xs text-warning">
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <span>{t('accounts.runtimeLoadFailed')}</span>
        </div>
      )}

      {/* 筛选条（含提供方 tab，取代原来独立的 provider Segment） */}
      <AccountsFilterBar
        statusFilter={statusFilter}
        onStatusChange={setStatusFilter}
        providerFilter={providerFilter}
        onProviderChange={setProviderFilter}
        providers={providers}
        tierFilter={tierFilter}
        onTierChange={setTierFilter}
        tiers={distinctTiers}
        showTierFilter={showTierFilter}
        sortKey={sortKey}
        onSortChange={setSortKey}
      />

      <AccountsTable
        data={pagedRows}
        loading={accountsQuery.isPending}
        runtimeByAccount={runtimeByAccount}
        runtimeState={runtimeState}
        groupColors={groupColors}
        quotaKind={quotaKind}
        showOnDemand={showOnDemand}
        busyId={busyId}
        onToggleDisabled={handleToggleDisabled}
        onEdit={(row) => setEditingId(row.account_id)}
        onDelete={handleDelete}
        onReset={handleReset}
        onRefresh={handleRefresh}
        onViewModels={(row) => setModelsAccountId(row.account_id)}
        onEditOnDemand={(row) => setOnDemandId(row.account_id)}
      />

      {/* 分页控件 */}
      <AccountsPagination
        page={currentPage}
        pageSize={pageSize}
        total={filteredRows.length}
        onPageChange={setPage}
        onPageSizeChange={(size) => {
          setPageSize(size)
          setPage(1)
        }}
      />

      <ImportAccountsDialog open={importOpen} onClose={() => setImportOpen(false)} />
      <OAuthAccountDialog open={oauthOpen} onClose={() => setOauthOpen(false)} />
      <CursorAccountDialog open={cursorOpen} onClose={() => setCursorOpen(false)} />
      <EditAccountDialog
        open={editingRow !== null}
        row={editingRow}
        onClose={() => setEditingId(null)}
      />
      <AccountModelsDialog
        open={modelsRow !== null}
        row={modelsRow}
        onClose={() => setModelsAccountId(null)}
      />
      <OnDemandDialog
        open={onDemandRow !== null}
        row={onDemandRow}
        current={onDemandCurrent}
        onClose={() => setOnDemandId(null)}
      />
    </div>
  )
}
