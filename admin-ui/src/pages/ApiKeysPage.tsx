import { useMemo, useState } from 'react'
import { Plus } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { ErrorNote } from '@/components/ui/error-note'
import { CreateKeyDialog } from '@/features/keys/components/CreateKeyDialog'
import { KeysTable } from '@/features/keys/components/KeysTable'
import { useDeleteKey, useKeys, useUpdateKey } from '@/features/keys/hooks'
import type { ApiKeyRow } from '@/features/keys/types'
import { useUsageByKey } from '@/features/usage/hooks'
import type { KeyUsage } from '@/features/usage/types'
import { useI18n } from '@/lib/i18n'

export default function ApiKeysPage() {
  const { t } = useI18n()
  const [dialogOpen, setDialogOpen] = useState(false)

  const keysQuery = useKeys()
  // 用量联表：复用 /usage/by-key?all=true，前端按 client_key_id === key join
  const usageQuery = useUsageByKey({ mode: 'all' })

  const usageByKey = useMemo(() => {
    const map = new Map<string, KeyUsage>()
    for (const row of usageQuery.data ?? []) map.set(row.client_key_id, row)
    return map
  }, [usageQuery.data])

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

  const actionError = updateMutation.isError
    ? updateMutation.error
    : deleteMutation.isError
      ? deleteMutation.error
      : null

  return (
    <div className="space-y-6">
      {/* Page hero：标题 + 新建入口 */}
      <div className="page-hero flex flex-wrap items-center justify-between gap-4 p-6">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">{t('keys.title')}</h1>
          <p className="mt-1 text-sm text-muted-foreground">{t('keys.subtitle')}</p>
        </div>
        <Button onClick={() => setDialogOpen(true)}>
          <Plus className="h-4 w-4" />
          {t('keys.new')}
        </Button>
      </div>

      {keysQuery.isError && <ErrorNote error={keysQuery.error} />}
      {/* 启停/删除失败时的提示（下一次操作发起时自动清除） */}
      {actionError !== null && <ErrorNote error={actionError} labelKey="common.actionFailed" />}

      <KeysTable
        data={keysQuery.data}
        loading={keysQuery.isPending}
        usageByKey={usageByKey}
        usageLoading={usageQuery.isPending}
        busyKey={busyKey}
        onToggleDisabled={handleToggleDisabled}
        onDelete={handleDelete}
        onSaveLabel={handleSaveLabel}
      />

      <CreateKeyDialog open={dialogOpen} onClose={() => setDialogOpen(false)} />
    </div>
  )
}
