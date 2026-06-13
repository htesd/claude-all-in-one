import type { LucideIcon } from 'lucide-react'
import type { ReactNode } from 'react'

import { Badge } from '@/components/ui/badge'
import { Card } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { useI18n } from '@/lib/i18n'

interface UsageTableCardProps {
  icon: LucideIcon
  title: string
  count?: number
  loading: boolean
  /** Column headers; `right: true` aligns the column to the right. */
  columns: { label: string; right?: boolean }[]
  /** Pre-rendered body rows (ignored while loading / empty). */
  rows: ReactNode[]
}

/** Shared glass card + table shell for the by-model / by-key usage tables. */
export function UsageTableCard({
  icon: Icon,
  title,
  count,
  loading,
  columns,
  rows,
}: UsageTableCardProps) {
  const { t } = useI18n()

  return (
    <Card className="overflow-hidden">
      <div className="flex items-center justify-between border-b border-black/5 px-5 py-4 dark:border-white/5">
        <div className="flex items-center gap-2">
          <Icon className="h-4 w-4 text-muted-foreground" />
          <h2 className="text-sm font-black tracking-[-0.01em]">{title}</h2>
        </div>
        {!loading && count !== undefined && <Badge variant="muted">{count}</Badge>}
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
            Array.from({ length: 4 }, (_, rowIndex) => (
              <TR key={rowIndex}>
                {columns.map((column) => (
                  <TD key={column.label}>
                    <Skeleton className={column.right ? 'ml-auto h-4 w-14' : 'h-4 w-24'} />
                  </TD>
                ))}
              </TR>
            ))
          ) : rows.length === 0 ? (
            <tr>
              <TD
                colSpan={columns.length}
                className="py-10 text-center text-muted-foreground"
              >
                {t('table.empty')}
              </TD>
            </tr>
          ) : (
            rows
          )}
        </TBody>
      </Table>
    </Card>
  )
}
