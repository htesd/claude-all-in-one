import { useEffect, useMemo, useState } from 'react'
import { ChevronLeft, ChevronRight, Download, RefreshCw } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { Modal } from '@/components/ui/modal'
import { Segment } from '@/components/ui/segment'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { useLogDetail, useLogs } from '@/features/logs/hooks'
import { PayloadView, type PayloadMode } from '@/features/logs/PayloadView'
import type { LogsSuccessFilter, LogsTimeRange } from '@/features/logs/types'
import { useI18n } from '@/lib/i18n'
import { cn, formatInt } from '@/lib/utils'

function formatDuration(ms: number | null): string {
  if (ms === null) return '—'
  return `${(ms / 1000).toFixed(1)}s`
}

/** 把完整报文文本下载为本地文件(大报文不便在弹窗里看时用)。 */
function downloadText(filename: string, content: string): void {
  const blob = new Blob([content], { type: 'application/json' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}

function formatTime(unixSec: number): string {
  return new Date(unixSec * 1000).toLocaleString('zh-CN')
}

/** 模型上下文窗口(token)。1M:opus 5、opus 4.6/4.7/4.8、sonnet 5 与 sonnet 4.6;其余按 200k。
 *  点号/连字符两种写法都归一(`claude-opus-4.8` 与 `claude-opus-4-8` 同窗口,审查 Architect#1),
 *  并兼容 -thinking / 日期快照后缀(子串匹配)。与后端 get_context_window_size 同档。
 *  ⚠️ 5 系锚定 `opus-5`/`sonnet-5` 邻接串:不能用裸 '5',否则 `claude-3-5-sonnet` 等历史名会被误判。 */
function contextWindow(model: string): number {
  const m = model.toLowerCase().replace(/\./g, '-')
  if (
    m.includes('opus-5') ||
    m.includes('opus-4-8') ||
    m.includes('opus-4-7') ||
    m.includes('opus-4-6') ||
    m.includes('sonnet-5') ||
    m.includes('sonnet-4-6')
  ) {
    return 1_000_000
  }
  return 200_000
}

/** credit:真号本次真实计费。0/缺省 → '—'(无 meteringEvent 信号)。 */
function formatCredit(c: number): string {
  return c > 0 ? c.toFixed(4) : '—'
}

/** num/denom 百分比(denom<=0 → '—')。 */
function ratioPct(num: number, denom: number, digits = 0): string {
  if (denom <= 0) return '—'
  return `${((num / denom) * 100).toFixed(digits)}%`
}

interface LogDetailModalProps {
  id: number | null
  onClose: () => void
}

function LogDetailModal({ id, onClose }: LogDetailModalProps) {
  const { t } = useI18n()
  const { data, isPending } = useLogDetail(id)
  const [payloadMode, setPayloadMode] = useState<PayloadMode>('formatted')

  const payloadModeOptions = [
    { value: 'formatted' as PayloadMode, label: t('logs.detail.viewFormatted') },
    { value: 'raw' as PayloadMode, label: t('logs.detail.viewRaw') },
  ]

  return (
    <Modal open={id !== null} onClose={onClose} title={t('logs.detail.title')} className="max-w-4xl">
      {isPending || !data ? (
        <div className="mt-4 space-y-3">
          <Skeleton className="h-24 w-full" />
          <Skeleton className="h-48 w-full" />
          <Skeleton className="h-48 w-full" />
        </div>
      ) : (
        <div className="mt-4 space-y-4">
          {/* Meta row */}
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.model')}
              </p>
              <p className="mt-0.5 truncate text-sm font-semibold">{data.model}</p>
            </div>
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.account')}
              </p>
              <p className="mt-0.5 truncate text-sm font-semibold">{data.account_id}</p>
            </div>
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.duration')}
              </p>
              <p className="mt-0.5 text-sm font-semibold">
                {formatDuration(data.duration_ms)}
                {data.ttfb_ms !== null && (
                  <span className="ml-1.5 text-xs font-normal text-muted-foreground">
                    TTFB {formatDuration(data.ttfb_ms)}
                  </span>
                )}
              </p>
            </div>
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.status')}
              </p>
              <div className="mt-1">
                <Badge variant={data.success ? 'success' : 'destructive'}>
                  {data.success ? t('logs.filter.success.ok') : t('logs.filter.success.fail')}
                </Badge>
              </div>
            </div>
          </div>

          {/* Token row */}
          <div className="grid grid-cols-3 gap-2">
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.input')}
              </p>
              <p className="mt-0.5 text-sm font-semibold">
                {formatInt(data.input_tokens)}
                <span className="ml-1.5 text-xs font-normal text-muted-foreground">
                  {ratioPct(data.input_tokens, contextWindow(data.model), 1)}
                </span>
              </p>
            </div>
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.output')}
              </p>
              <p className="mt-0.5 text-sm font-semibold">{formatInt(data.output_tokens)}</p>
            </div>
            {/* credit:真号本次真实计费(Kiro 原生 metering) */}
            <div className="rounded-2xl bg-acid/15 px-3 py-2">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.credit')}
              </p>
              <p className="mt-0.5 text-sm font-semibold">{formatCredit(data.metering_credit)}</p>
            </div>
            {/* 真:Kiro 服务端真实命中(优化目标) */}
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.real')}
              </p>
              <p className="mt-0.5 text-sm font-semibold">
                {data.real_cache_read_tokens > 0 ? (
                  <>
                    {formatInt(data.real_cache_read_tokens)}
                    <span className="ml-1.5 text-xs font-normal text-muted-foreground">
                      {ratioPct(data.real_cache_read_tokens, data.input_tokens, 0)}
                    </span>
                  </>
                ) : (
                  <span className="text-muted-foreground">{t('logs.miss')}</span>
                )}
              </p>
            </div>
            {/* 报:上报给 NewAPI 的缓存(计费口径) */}
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.reported')}
              </p>
              <p className="mt-0.5 text-sm font-semibold">{formatInt(data.cache_read_tokens)}</p>
            </div>
            {/* 上下文占比 = input / 模型窗口 */}
            <div className="rounded-2xl bg-black/5 px-3 py-2 dark:bg-white/5">
              <p className="text-[10px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                {t('logs.detail.context')}
              </p>
              <p className="mt-0.5 text-sm font-semibold">
                {ratioPct(data.input_tokens, contextWindow(data.model), 1)}
              </p>
            </div>
          </div>

          {/* Payload blocks */}
          <div className="space-y-3">
            <div className="flex justify-end">
              <Segment options={payloadModeOptions} value={payloadMode} onChange={setPayloadMode} />
            </div>
            <div>
              <div className="mb-1.5 flex items-center justify-between">
                <p className="text-xs font-semibold text-muted-foreground">
                  {t('logs.detail.clientPayload')}
                </p>
                <button
                  type="button"
                  onClick={() => downloadText(`client-${data.id}.json`, data.client_payload)}
                  className="inline-flex items-center gap-1 rounded-lg px-2 py-1 text-[11px] text-muted-foreground hover:bg-black/5 dark:hover:bg-white/5"
                >
                  <Download className="h-3 w-3" />
                  {t('logs.detail.download')}
                </button>
              </div>
              <PayloadView raw={data.client_payload} mode={payloadMode} blobs={data.blobs} />
            </div>
            <div>
              <div className="mb-1.5 flex items-center justify-between">
                <p className="text-xs font-semibold text-muted-foreground">
                  {t('logs.detail.kiroPayload')}
                </p>
                <button
                  type="button"
                  onClick={() => downloadText(`kiro-${data.id}.json`, data.kiro_payload)}
                  className="inline-flex items-center gap-1 rounded-lg px-2 py-1 text-[11px] text-muted-foreground hover:bg-black/5 dark:hover:bg-white/5"
                >
                  <Download className="h-3 w-3" />
                  {t('logs.detail.download')}
                </button>
              </div>
              <PayloadView raw={data.kiro_payload} mode={payloadMode} blobs={data.blobs} />
            </div>
            {/* 模型回复:仅成功且采集到时展示(旧日志/失败请求无此字段) */}
            {data.response_payload && (
              <div>
                <div className="mb-1.5 flex items-center justify-between">
                  <p className="text-xs font-semibold text-muted-foreground">
                    {t('logs.detail.responsePayload')}
                  </p>
                  <button
                    type="button"
                    onClick={() => downloadText(`response-${data.id}.json`, data.response_payload)}
                    className="inline-flex items-center gap-1 rounded-lg px-2 py-1 text-[11px] text-muted-foreground hover:bg-black/5 dark:hover:bg-white/5"
                  >
                    <Download className="h-3 w-3" />
                    {t('logs.detail.download')}
                  </button>
                </div>
                <PayloadView raw={data.response_payload} mode={payloadMode} blobs={data.blobs} />
              </div>
            )}
          </div>
        </div>
      )}
    </Modal>
  )
}

export default function RequestLogsPage() {
  const { t } = useI18n()

  const [successFilter, setSuccessFilter] = useState<LogsSuccessFilter>('all')
  const [timeRange, setTimeRange] = useState<LogsTimeRange>(7)
  const [accountInput, setAccountInput] = useState('')
  const [modelInput, setModelInput] = useState('')
  const [accountFilter, setAccountFilter] = useState<string | undefined>(undefined)
  const [modelFilter, setModelFilter] = useState<string | undefined>(undefined)
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [page, setPage] = useState(1)

  const filter = useMemo(
    () => ({
      success: successFilter,
      timeRange,
      ...(accountFilter ? { account: accountFilter } : {}),
      ...(modelFilter ? { model: modelFilter } : {}),
    }),
    [successFilter, timeRange, accountFilter, modelFilter],
  )

  // 筛选条件变化时回到第一页(filter 仅在依赖变化时换引用)。
  useEffect(() => setPage(1), [filter])

  const { data, isPending, isError, error, refetch } = useLogs(filter, page)
  const rows = data?.items ?? []
  const total = data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / (data?.page_size || 50)))

  const successOptions = [
    { value: 'all' as LogsSuccessFilter, label: t('logs.filter.success.all') },
    { value: 'ok' as LogsSuccessFilter, label: t('logs.filter.success.ok') },
    { value: 'fail' as LogsSuccessFilter, label: t('logs.filter.success.fail') },
  ]

  const timeOptions = [
    { value: 7 as LogsTimeRange, label: t('range.7d') },
    { value: 30 as LogsTimeRange, label: t('range.30d') },
    { value: 'all' as LogsTimeRange, label: t('range.all') },
  ]

  const applyAccountFilter = () => {
    const trimmed = accountInput.trim()
    setAccountFilter(trimmed || undefined)
  }

  const applyModelFilter = () => {
    const trimmed = modelInput.trim()
    setModelFilter(trimmed || undefined)
  }

  return (
    <div className="space-y-6">
      {/* Page hero */}
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <p className="eyebrow">{t('logs.eyebrow')}</p>
          <h1 className="mt-2 font-display text-4xl font-black tracking-[-0.04em]">
            {t('logs.title')}
          </h1>
          <p className="mt-2 text-sm text-muted-foreground">{t('logs.subtitle')}</p>
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={() => void refetch()}
          className="flex items-center gap-1.5"
        >
          <RefreshCw className="h-3.5 w-3.5" />
          {t('logs.refresh')}
        </Button>
      </header>

      {/* Filter bar */}
      <Card className="flex flex-wrap items-center gap-3 px-4 py-3">
        <Segment options={successOptions} value={successFilter} onChange={setSuccessFilter} />
        <Segment options={timeOptions} value={timeRange} onChange={setTimeRange} />
        <input
          type="text"
          value={accountInput}
          onChange={(e) => setAccountInput(e.target.value)}
          onBlur={applyAccountFilter}
          onKeyDown={(e) => e.key === 'Enter' && applyAccountFilter()}
          placeholder={t('logs.filter.account')}
          className="rounded-2xl border bg-input px-3 py-1.5 text-sm focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
        />
        <input
          type="text"
          value={modelInput}
          onChange={(e) => setModelInput(e.target.value)}
          onBlur={applyModelFilter}
          onKeyDown={(e) => e.key === 'Enter' && applyModelFilter()}
          placeholder={t('logs.filter.model')}
          className="rounded-2xl border bg-input px-3 py-1.5 text-sm focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
        />
      </Card>

      {isError && <ErrorNote error={error} />}

      {/* Logs table */}
      <Card className="overflow-hidden">
        <Table>
          <THead>
            <tr>
              <TH>{t('logs.col.time')}</TH>
              <TH>{t('logs.col.status')}</TH>
              <TH>{t('logs.col.model')}</TH>
              <TH>{t('logs.col.endpoint')}</TH>
              <TH>{t('logs.col.account')}</TH>
              <TH className="text-right">{t('logs.col.duration')}</TH>
              <TH className="text-right">{t('logs.col.input')}</TH>
              <TH className="text-right">{t('logs.col.output')}</TH>
              <TH className="text-right">{t('logs.col.real')}</TH>
              <TH className="text-right">{t('logs.col.reported')}</TH>
              <TH className="text-right">{t('logs.col.credit')}</TH>
            </tr>
          </THead>
          <TBody>
            {isPending ? (
              Array.from({ length: 8 }).map((_, i) => (
                <TR key={i}>
                  {Array.from({ length: 11 }).map((__, j) => (
                    <TD key={j}>
                      <Skeleton className="h-4 w-full" />
                    </TD>
                  ))}
                </TR>
              ))
            ) : rows.length === 0 ? (
              <TR>
                <TD colSpan={11} className="py-10 text-center text-sm text-muted-foreground">
                  {t('logs.empty')}
                </TD>
              </TR>
            ) : (
              rows.map((row) => (
                <TR
                  key={row.id}
                  className={cn('cursor-pointer')}
                  onClick={() => setSelectedId(row.id)}
                >
                  <TD className="text-xs text-muted-foreground">{formatTime(row.created_at)}</TD>
                  <TD>
                    <Badge variant={row.success ? 'success' : 'destructive'}>
                      {row.success ? t('logs.filter.success.ok') : t('logs.filter.success.fail')}
                    </Badge>
                  </TD>
                  <TD className="max-w-[200px] truncate font-mono text-xs">{row.model}</TD>
                  <TD className="text-xs">
                    <span>/v1/messages</span>
                    {row.stream && (
                      <span className="ml-1.5 text-muted-foreground">· 流式</span>
                    )}
                  </TD>
                  <TD className="font-mono text-xs">{row.account_id}</TD>
                  <TD className="text-right text-xs">
                    <span>{formatDuration(row.duration_ms)}</span>
                    {row.ttfb_ms !== null && (
                      <span className="block text-muted-foreground">
                        TTFB {formatDuration(row.ttfb_ms)}
                      </span>
                    )}
                  </TD>
                  <TD className="text-right font-mono text-xs">
                    {formatInt(row.input_tokens)}
                    <span className="block font-sans text-[10px] text-muted-foreground">
                      {ratioPct(row.input_tokens, contextWindow(row.model), 1)}
                    </span>
                  </TD>
                  <TD className="text-right font-mono text-xs">
                    {formatInt(row.output_tokens)}
                  </TD>
                  {/* 真:Kiro 服务端真实命中(真号真实消耗的依据);0 → miss */}
                  <TD className="text-right font-mono text-xs">
                    {row.real_cache_read_tokens > 0 ? (
                      <>
                        {formatInt(row.real_cache_read_tokens)}
                        <span className="block font-sans text-[10px] text-muted-foreground">
                          {ratioPct(row.real_cache_read_tokens, row.input_tokens, 0)}
                        </span>
                      </>
                    ) : (
                      <span className="text-muted-foreground">{t('logs.miss')}</span>
                    )}
                  </TD>
                  {/* 报:上报给 NewAPI 的缓存(计费口径) */}
                  <TD className="text-right font-mono text-xs text-muted-foreground">
                    {formatInt(row.cache_read_tokens)}
                  </TD>
                  {/* credit:Kiro 原生计费 = 真号本次真实消耗 */}
                  <TD className="text-right font-mono text-xs font-semibold">
                    {formatCredit(row.metering_credit)}
                  </TD>
                </TR>
              ))
            )}
          </TBody>
        </Table>
      </Card>

      {/* Pagination */}
      {total > 0 && (
        <div className="flex items-center justify-between text-xs text-muted-foreground">
          <span>{t('logs.page.total', { total, page, totalPages })}</span>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              disabled={page <= 1}
              onClick={() => setPage((p) => Math.max(1, p - 1))}
              className="flex items-center gap-1"
            >
              <ChevronLeft className="h-3.5 w-3.5" />
              {t('logs.page.prev')}
            </Button>
            <Button
              variant="outline"
              size="sm"
              disabled={page >= totalPages}
              onClick={() => setPage((p) => Math.min(totalPages, p + 1))}
              className="flex items-center gap-1"
            >
              {t('logs.page.next')}
              <ChevronRight className="h-3.5 w-3.5" />
            </Button>
          </div>
        </div>
      )}

      {/* Detail modal */}
      <LogDetailModal id={selectedId} onClose={() => setSelectedId(null)} />
    </div>
  )
}
