import { useMemo, useState } from 'react'
import { AlertTriangle, Plus, Upload } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { ErrorNote } from '@/components/ui/error-note'
import { AccountsTable } from '@/features/accounts/components/AccountsTable'
import type { RuntimeQueryState } from '@/features/accounts/components/AccountTableRow'
import { CreateAccountDialog } from '@/features/accounts/components/CreateAccountDialog'
import { EditAccountDialog } from '@/features/accounts/components/EditAccountDialog'
import { ImportAccountsDialog } from '@/features/accounts/components/ImportAccountsDialog'
import {
  useAccounts,
  useAccountsRuntime,
  useDeleteAccount,
  useResetAccount,
  useUpdateAccount,
} from '@/features/accounts/hooks'
import { mergeRuntimeByAccount } from '@/features/accounts/lib'
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

  const [createOpen, setCreateOpen] = useState(false)
  const [importOpen, setImportOpen] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null)

  const groupColors = useMemo(() => {
    const map = new Map<string, string>()
    for (const group of groupsQuery.data ?? []) map.set(group.name, group.color)
    return map
  }, [groupsQuery.data])

  const runtimeByAccount = useMemo(
    () => mergeRuntimeByAccount(runtimeQuery.data),
    [runtimeQuery.data],
  )

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
        : null

  const handleToggleDisabled = (row: AccountRow) =>
    updateMutation.mutate({ id: row.account_id, patch: { disabled: !row.disabled } })
  const handleDelete = (id: string) => deleteMutation.mutate(id)
  const handleReset = (id: string) => resetMutation.mutate(id)

  const actionError = updateMutation.isError
    ? updateMutation.error
    : deleteMutation.isError
      ? deleteMutation.error
      : resetMutation.isError
        ? resetMutation.error
        : null

  return (
    <div className="space-y-6">
      {/* Page hero：标题 + 添加入口 */}
      <div className="page-hero flex flex-wrap items-center justify-between gap-4 p-6">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">{t('accounts.title')}</h1>
          <p className="mt-1 text-sm text-muted-foreground">{t('accounts.subtitle')}</p>
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
      </div>

      {accountsQuery.isError && <ErrorNote error={accountsQuery.error} />}
      {/* 启停/删除失败时的提示（下一次操作发起时自动清除） */}
      {actionError !== null && <ErrorNote error={actionError} labelKey="common.actionFailed" />}

      {/* runtime 失败不致命：账号列表照常展示，但要明示状态列不可信 */}
      {runtimeQuery.isError && (
        <div className="flex items-center gap-1.5 px-1 text-xs text-warning">
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <span>{t('accounts.runtimeLoadFailed')}</span>
        </div>
      )}

      <AccountsTable
        data={accountsQuery.data}
        loading={accountsQuery.isPending}
        runtimeByAccount={runtimeByAccount}
        runtimeState={runtimeState}
        groupColors={groupColors}
        busyId={busyId}
        onToggleDisabled={handleToggleDisabled}
        onEdit={(row) => setEditingId(row.account_id)}
        onDelete={handleDelete}
        onReset={handleReset}
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
