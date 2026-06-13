import { useMemo, useState } from 'react'
import { AlertTriangle, Plus } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { ErrorNote } from '@/components/ui/error-note'
import { useGroups } from '@/features/groups/hooks'
import { CreateKeyDialog } from '@/features/keys/components/CreateKeyDialog'
import { KeyManageDialog } from '@/features/keys/components/KeyManageDialog'
import { KeysTable } from '@/features/keys/components/KeysTable'
import { useDeleteKey, useKeys, useUpdateKey } from '@/features/keys/hooks'
import type { ApiKeyRow } from '@/features/keys/types'
import { useUsageByKey } from '@/features/usage/hooks'
import type { KeyUsage } from '@/features/usage/types'
import { useI18n } from '@/lib/i18n'

export default function ApiKeysPage() {
  const { t } = useI18n()
  const [dialogOpen, setDialogOpen] = useState(false)
  /** 「限额与分组」对话框当前管理的 key；null = 关闭。 */
  const [manageKey, setManageKey] = useState<string | null>(null)

  const keysQuery = useKeys()
  // 用量联表：复用 /usage/by-key?all=true，前端按 client_key_id === key join
  const usageQuery = useUsageByKey({ mode: 'all' })
  // 分组列上色 + 对话框里的分组 select
  const groupsQuery = useGroups()

  const usageByKey = useMemo(() => {
    const map = new Map<string, KeyUsage>()
    for (const row of usageQuery.data ?? []) map.set(row.client_key_id, row)
    return map
  }, [usageQuery.data])

  const groupColors = useMemo(() => {
    const map = new Map<string, string>()
    for (const group of groupsQuery.data ?? []) map.set(group.name, group.color)
    return map
  }, [groupsQuery.data])

  const updateMutation = useUpdateKey()
  const deleteMutation = useDeleteKey()

  // 当前有 mutation 进行中的 key —— 只置灰对应行的按钮，其他行可继续操作
  const busyKey = updateMutation.isPending
    ? (updateMutation.variables?.key ?? null)
    : deleteMutation.isPending
      ? (deleteMutation.variables ?? null)
      : null

  const handleToggleDisabled = (row: ApiKeyRow) =>
    updateMutation.mutate({ key: row.key, patch: { disabled: !row.disabled } })
  const handleDelete = (key: string) => deleteMutation.mutate(key)
  // label 传空串 = 清空备注（后端 PATCH 语义）
  const handleSaveLabel = (key: string, label: string) =>
    updateMutation.mutate({ key, patch: { label } })

  // 管理对话框跟随列表数据（refetch 后行被删则自动关闭）
  const manageRow =
    manageKey !== null ? (keysQuery.data?.find((row) => row.key === manageKey) ?? null) : null

  const actionError = updateMutation.isError
    ? updateMutation.error
    : deleteMutation.isError
      ? deleteMutation.error
      : null

  return (
    <div className="space-y-6">
      {/* Page hero：标题 + 新建入口 */}
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <p className="eyebrow">API Keys</p>
          <h1 className="mt-2 font-display text-4xl font-black tracking-[-0.04em]">{t('keys.title')}</h1>
          <p className="mt-2 text-sm text-muted-foreground">{t('keys.subtitle')}</p>
        </div>
        <Button onClick={() => setDialogOpen(true)}>
          <Plus className="h-4 w-4" />
          {t('keys.new')}
        </Button>
      </header>

      {keysQuery.isError && <ErrorNote error={keysQuery.error} />}
      {/* 启停/删除失败时的提示（下一次操作发起时自动清除） */}
      {actionError !== null && <ErrorNote error={actionError} labelKey="common.actionFailed" />}

      {/* 用量联表失败不致命：key 列表照常展示，但要明示用量列不可信，避免被当成"没用量" */}
      {usageQuery.isError && (
        <div className="flex items-center gap-1.5 px-1 text-xs text-warning">
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <span>{t('keys.usageLoadFailed')}</span>
        </div>
      )}

      <KeysTable
        data={keysQuery.data}
        loading={keysQuery.isPending}
        usageByKey={usageByKey}
        usageLoading={usageQuery.isPending}
        groupColors={groupColors}
        busyKey={busyKey}
        onToggleDisabled={handleToggleDisabled}
        onDelete={handleDelete}
        onSaveLabel={handleSaveLabel}
        onManage={(row) => setManageKey(row.key)}
      />

      <CreateKeyDialog open={dialogOpen} onClose={() => setDialogOpen(false)} />
      <KeyManageDialog
        open={manageRow !== null}
        row={manageRow}
        groups={groupsQuery.data ?? []}
        onClose={() => setManageKey(null)}
      />
    </div>
  )
}
