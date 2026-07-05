import { Button } from '@/components/ui/button'
import { Select } from '@/components/ui/select'
import { useI18n } from '@/lib/i18n'

const PAGE_SIZE_OPTIONS = [20, 50, 100] as const
export type PageSize = (typeof PAGE_SIZE_OPTIONS)[number]

interface AccountsPaginationProps {
  page: number
  pageSize: PageSize
  total: number
  onPageChange: (page: number) => void
  onPageSizeChange: (size: PageSize) => void
}

/** 分页控件：每页条数选择 + 上/下一页 + 汇总行。 */
export function AccountsPagination({
  page,
  pageSize,
  total,
  onPageChange,
  onPageSizeChange,
}: AccountsPaginationProps) {
  const { t } = useI18n()
  const totalPages = Math.max(1, Math.ceil(total / pageSize))
  const clampedPage = Math.min(Math.max(1, page), totalPages)

  const summary = t('pagination.summary', {
    page: clampedPage,
    totalPages,
    total,
  })

  return (
    <div className="flex flex-wrap items-center justify-between gap-3 px-1 text-xs text-muted-foreground">
      {/* 左侧：每页条数 */}
      <div className="flex items-center gap-2">
        <span>{t('pagination.pageSize')}</span>
        <Select
          value={pageSize}
          onChange={(e) => onPageSizeChange(Number(e.target.value) as PageSize)}
          className="w-20"
        >
          {PAGE_SIZE_OPTIONS.map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
        </Select>
      </div>

      {/* 右侧：上一页 / 摘要 / 下一页 */}
      <div className="flex items-center gap-2">
        {totalPages > 1 && (
          <Button
            variant="outline"
            size="sm"
            disabled={clampedPage <= 1}
            onClick={() => onPageChange(clampedPage - 1)}
          >
            {t('pagination.prev')}
          </Button>
        )}
        <span className="tabular-nums">{summary}</span>
        {totalPages > 1 && (
          <Button
            variant="outline"
            size="sm"
            disabled={clampedPage >= totalPages}
            onClick={() => onPageChange(clampedPage + 1)}
          >
            {t('pagination.next')}
          </Button>
        )}
      </div>
    </div>
  )
}
