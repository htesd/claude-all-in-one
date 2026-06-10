import { KeyRound } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Card } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import type { KeyUsage } from '@/features/usage/types'
import { useI18n } from '@/lib/i18n'

import type { ApiKeyRow } from '../types'
import { KeyRow } from './KeyRow'

interface KeysTableProps {
  data: ApiKeyRow[] | undefined
  loading: boolean
  /** client_key_id -> 全时段用量，前端 join。 */
  usageByKey: Map<string, KeyUsage>
  usageLoading: boolean
  /** 当前有 mutation 进行中的 key（仅该行操作置灰）。 */
  busyKey: string | null
  onToggleDisabled: (row: ApiKeyRow) => void
  onDelete: (key: string) => void
  onSaveLabel: (key: string, label: string) => void
}

/**
 * Key 列表玻璃卡表格。不复用 UsageTableCard 是因为这里需要
 * 自定义空态引导文案 + 带交互的行（行内编辑 / 删除确认）。
 */
export function KeysTable({
  data,
  loading,
  usageByKey,
  usageLoading,
  busyKey,
  onToggleDisabled,
  onDelete,
  onSaveLabel,
}: KeysTableProps) {
  const { t } = useI18n()

  const columns = [
    { label: t('table.key') },
    { label: t('table.label') },
    { label: t('table.status') },
    { label: t('table.createdAt') },
    { label: t('table.requests'), right: true },
    { label: t('table.tokens'), right: true },
    { label: t('table.actions'), right: true },
  ]

  const rows = data ?? []

  return (
    <Card className="overflow-hidden">
      <div className="flex items-center justify-between border-b border-black/5 px-5 py-4 dark:border-white/5">
        <div className="flex items-center gap-2">
          <KeyRound className="h-4 w-4 text-primary" />
          <h2 className="text-sm font-semibold">{t('keys.listTitle')}</h2>
        </div>
        {!loading && <Badge variant="muted">{rows.length}</Badge>}
      </div>
      <Table>
        <THead>
          <tr>
            {columns.map((column) => (
              <TH key={column.label} className={column.right ? 'text-right' : undefined}>
                {column.label}
              </TH>
            ))}
          </tr>
        </THead>
        <TBody>
          {loading ? (
            Array.from({ length: 3 }, (_, rowIndex) => (
              <TR key={rowIndex}>
                {columns.map((column) => (
                  <TD key={column.label}>
                    <Skeleton className={column.right ? 'ml-auto h-4 w-14' : 'h-4 w-20'} />
                  </TD>
                ))}
              </TR>
            ))
          ) : rows.length === 0 ? (
            <tr>
              <TD colSpan={columns.length} className="py-10 text-center text-muted-foreground">
                {t('keys.empty')}
              </TD>
            </tr>
          ) : (
            rows.map((row) => (
              <KeyRow
                key={row.key}
                row={row}
                usage={usageByKey.get(row.key)}
                usageLoading={usageLoading}
                busy={busyKey === row.key}
                onToggleDisabled={onToggleDisabled}
                onDelete={onDelete}
                onSaveLabel={onSaveLabel}
              />
            ))
          )}
        </TBody>
      </Table>
    </Card>
  )
}
