import { useEffect, useMemo, useState } from 'react'
import { AlertTriangle, CheckCircle2, Plus, Upload } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { ErrorNote } from '@/components/ui/error-note'
import { Segment } from '@/components/ui/segment'
import { AccountsTable } from '@/features/accounts/components/AccountsTable'
import type { RuntimeQueryState } from '@/features/accounts/components/AccountTableRow'
import { CreateAccountDialog } from '@/features/accounts/components/CreateAccountDialog'
import { EditAccountDialog } from '@/features/accounts/components/EditAccountDialog'
import { ImportAccountsDialog } from '@/features/accounts/components/ImportAccountsDialog'
import {
  useAccounts,
  useAccountsRuntime,
  useDeleteAccount,
  useRefreshAccount,
  useResetAccount,
  useUpdateAccount,
} from '@/features/accounts/hooks'
import type { RefreshAccountResult } from '@/features/accounts/api'
import { mergeRuntimeByAccount, providerTabLabel, quotaKindForProvider } from '@/features/accounts/lib'
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

  const [createOpen, setCreateOpen] = useState(false)
  const [importOpen, setImportOpen] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null)
  // 刷新 token 成功的轻量反馈（无 toast 库）：账号 + 新有效期。本地态而非派生自
  // refreshMutation.isSuccess——后者会在其它 mutation 成功后残留旧账号（审查 3 名 reviewer）。
  // 任一操作（含再次刷新）开始即清空，做到注释承诺的"下次操作自动消失"。
  const [refreshOk, setRefreshOk] = useState<RefreshAccountResult | null>(null)

  const groupColors = useMemo(() => {
    const map = new Map<string, string>()
    for (const group of groupsQuery.data ?? []) map.set(group.name, group.color)
    return map
  }, [groupsQuery.data])

  const runtimeByAccount = useMemo(
    () => mergeRuntimeByAccount(runtimeQuery.data),
    [runtimeQuery.data],
  )

  // 按 provider 分 Tab:不同 provider 配额口径不同(Kiro=积分,ccmax/dario=5h/7d 利用率),
  // 混在一张表里列头无法两全。统计在册的 provider(含计数),kiro→ccmax→其余 排序。
  const providers = useMemo(() => {
    const counts = new Map<string, number>()
    for (const row of accountsQuery.data ?? []) {
      counts.set(row.provider, (counts.get(row.provider) ?? 0) + 1)
    }
    const rank = (p: string) => (p === 'kiro' ? 0 : p === 'claude-dario' ? 1 : 2)
    return [...counts.entries()]
      .sort((a, b) => rank(a[0]) - rank(b[0]) || a[0].localeCompare(b[0]))
      .map(([provider, count]) => ({ provider, count }))
  }, [accountsQuery.data])

  // 用户选中的 tab;未选/已不存在时回落到首个 provider(prefer kiro,见 rank 排序)。
  const [activeProvider, setActiveProvider] = useState<string | null>(null)
  // 选中的 provider 从列表消失(如删光该组账号)时清空选择,回落首个 provider。否则
  // 该 provider 日后重新出现会把用户从当前 tab 意外弹回旧 tab(审查 Skeptic#1)。
  useEffect(() => {
    if (activeProvider !== null && !providers.some((p) => p.provider === activeProvider)) {
      setActiveProvider(null)
    }
  }, [providers, activeProvider])
  const effectiveProvider =
    activeProvider !== null && providers.some((p) => p.provider === activeProvider)
      ? activeProvider
      : (providers[0]?.provider ?? '')

  const visibleRows = useMemo(
    () => (accountsQuery.data ?? []).filter((row) => row.provider === effectiveProvider),
    [accountsQuery.data, effectiveProvider],
  )
  const quotaKind = quotaKindForProvider(effectiveProvider)

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
        <div className="flex items-center gap-2">
          <Button variant="outline" onClick={() => setImportOpen(true)}>
            <Upload className="h-4 w-4" />
            {t('accounts.import')}
          </Button>
          <Button onClick={() => setCreateOpen(true)}>
            <Plus className="h-4 w-4" />
            {t('accounts.new')}
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

      {/* runtime 失败不致命：账号列表照常展示，但要明示状态列不可信 */}
      {runtimeQuery.isError && (
        <div className="flex items-center gap-1.5 px-1 text-xs text-warning">
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <span>{t('accounts.runtimeLoadFailed')}</span>
        </div>
      )}

      {/* provider 分 Tab(≥2 个 provider 才显示):Kiro 显示积分,ccmax 显示 5h/7d 限额。 */}
      {providers.length >= 2 && (
        <Segment
          options={providers.map((p) => ({
            value: p.provider,
            label: `${providerTabLabel(p.provider)} (${p.count})`,
          }))}
          value={effectiveProvider}
          onChange={setActiveProvider}
        />
      )}

      <AccountsTable
        data={visibleRows}
        loading={accountsQuery.isPending}
        runtimeByAccount={runtimeByAccount}
        runtimeState={runtimeState}
        groupColors={groupColors}
        quotaKind={quotaKind}
        busyId={busyId}
        onToggleDisabled={handleToggleDisabled}
        onEdit={(row) => setEditingId(row.account_id)}
        onDelete={handleDelete}
        onReset={handleReset}
        onRefresh={handleRefresh}
      />

      <CreateAccountDialog open={createOpen} onClose={() => setCreateOpen(false)} />
      <ImportAccountsDialog open={importOpen} onClose={() => setImportOpen(false)} />
      <EditAccountDialog
        open={editingRow !== null}
        row={editingRow}
        onClose={() => setEditingId(null)}
      />
    </div>
  )
}
