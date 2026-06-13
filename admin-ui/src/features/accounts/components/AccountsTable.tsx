import { Users } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Card } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { useI18n } from '@/lib/i18n'

import type { AccountRow, AccountRuntimeEntry } from '../types'
import { AccountTableRow, type RuntimeQueryState } from './AccountTableRow'

interface AccountsTableProps {
  data: AccountRow[] | undefined
  loading: boolean
  /** account_id -> 运行态（mergeRuntimeByAccount 的结果）。 */
  runtimeByAccount: Map<string, AccountRuntimeEntry>
  runtimeState: RuntimeQueryState
  /** 分组名 -> 颜色（来自 /groups），分组列上色用。 */
  groupColors: Map<string, string>
  /** 当前有 mutation 进行中的 account_id（仅该行操作置灰）。 */
  busyId: string | null
  onToggleDisabled: (row: AccountRow) => void
  onEdit: (row: AccountRow) => void
  onDelete: (id: string) => void
  onReset: (id: string) => void
  onRefresh: (id: string) => void
}

/** 账号列表玻璃卡表格：配置行 + worker 运行态的 merge 视图。 */
export function AccountsTable({
  data,
  loading,
  runtimeByAccount,
  runtimeState,
  groupColors,
  busyId,
  onToggleDisabled,
  onEdit,
  onDelete,
  onReset,
  onRefresh,
}: AccountsTableProps) {
  const { t } = useI18n()

  const columns = [
    { label: t('table.accountId') },
    { label: t('table.group') },
    { label: t('table.provider') },
    { label: t('table.status') },
    { label: t('table.credits'), right: true },
    { label: t('table.concurrency'), right: true },
    { label: t('table.failures'), right: true },
    { label: t('table.actions'), right: true },
  ]

  const rows = data ?? []

  return (
    <Card className="overflow-hidden">
      <div className="flex items-center justify-between border-b border-black/5 px-5 py-4 dark:border-white/5">
        <div className="flex items-center gap-2">
          <Users className="h-4 w-4 text-primary" />
          <h2 className="text-sm font-semibold">{t('accounts.listTitle')}</h2>
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
                {t('accounts.empty')}
              </TD>
            </tr>
          ) : (
            rows.map((row) => (
              <AccountTableRow
                key={row.account_id}
                row={row}
                runtime={runtimeByAccount.get(row.account_id)}
                runtimeState={runtimeState}
                groupColor={groupColors.get(row.group_name)}
                busy={busyId === row.account_id}
                onToggleDisabled={onToggleDisabled}
                onEdit={onEdit}
                onDelete={onDelete}
                onReset={onReset}
                onRefresh={onRefresh}
              />
            ))
          )}
        </TBody>
      </Table>
    </Card>
  )
}
