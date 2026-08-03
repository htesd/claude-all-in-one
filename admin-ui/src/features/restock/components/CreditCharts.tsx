import { useMemo, useState } from 'react'

import { Segment } from '@/components/ui/segment'
import { useI18n } from '@/lib/i18n'

import type { RestockCredits } from '../types'

/**
 * 图表用 CSS 柱而非 SVG。
 *
 * SVG 里的文字会跟着 viewBox 缩放，375px 窄屏上刻度会缩到 6px 不可读；flex 柱天然等宽、
 * 字号始终是真实字号。这也和本仓既有的可视化语言一致（用量表里的占比条同理）。
 */

/** 时间轴最多渲染多少根柱子。7 天 = 168 根，间隙就占满整个宽度，必须并桶。 */
const MAX_COLS = 56

interface Col {
  ts: number
  ksk: number
  credits: number
  calls: number
  ksk_calls: number
  partial: boolean
  forecast: boolean
  span: number
  basis?: string
  samples?: number
  tsEnd: number
}

/** 把逐小时序列并成不超过 max 根。桶内**求和**（柱高表示这段时间烧了多少）。 */
function bucketize(cols: Col[], max: number): Col[] {
  const k = Math.ceil(cols.length / max)
  if (k <= 1) return cols
  const out: Col[] = []
  for (let i = 0; i < cols.length; i += k) {
    const g = cols.slice(i, i + k)
    out.push({
      ts: g[0].ts,
      tsEnd: g[g.length - 1].tsEnd,
      span: g.length,
      ksk: +g.reduce((a, x) => a + x.ksk, 0).toFixed(1),
      credits: +g.reduce((a, x) => a + x.credits, 0).toFixed(1),
      calls: g.reduce((a, x) => a + x.calls, 0),
      ksk_calls: g.reduce((a, x) => a + x.ksk_calls, 0),
      partial: g.some((x) => x.partial),
      // 混了预测的桶一律按预测显示 —— 宁可提醒「这根别全信」，
      // 也不能把猜出来的画成量到的。
      forecast: g.some((x) => x.forecast),
      basis: g.find((x) => x.basis)?.basis,
      samples: g.find((x) => x.samples)?.samples,
    })
  }
  return out
}

function fmtLocal(ts: number, offsetMinutes: number): string {
  const d = new Date((ts + offsetMinutes * 60) * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())} ${p(d.getUTCHours())}:00`
}

/** 高峰窗口按整点判定；end < start 表示跨零点（如 09:00–02:00）。 */
function inPeak(hour: number, start: string, end: string): boolean {
  const s = Number(start.slice(0, 2))
  const e = Number(end.slice(0, 2))
  if (Number.isNaN(s) || Number.isNaN(e) || s === e) return true
  return s < e ? hour >= s && hour < e : hour >= s || hour < e
}

const WD = ['一', '二', '三', '四', '五', '六', '日']

export function CreditCharts({ data }: { data: RestockCredits }) {
  const { t } = useI18n()
  const [profMode, setProfMode] = useState<'hour' | 'week'>('hour')
  const [pick, setPick] = useState<string | null>(null)
  const off = data.utc_offset_minutes

  // ① 时间轴：实测柱 + 预测柱接在右边，同一根轴上。
  // 分成两张图就没法一眼看出「接下来比刚才忙还是闲」，而那正是要不要补号的依据。
  const cols = useMemo(() => {
    const measured: Col[] = data.series.map((s) => ({
      ts: s.ts,
      tsEnd: s.ts + 3600,
      span: 1,
      ksk: s.ksk,
      credits: s.credits,
      calls: s.calls,
      ksk_calls: s.ksk_calls,
      partial: s.partial,
      forecast: false,
    }))
    const predicted: Col[] = data.forecast.map((f) => ({
      ts: f.ts,
      tsEnd: f.ts + 3600,
      span: 1,
      // 预测是**总需求**，所以整根柱都按 ksk 段画（视觉上与实测的总高度可比）。
      ksk: f.credits,
      credits: f.credits,
      calls: 0,
      ksk_calls: 0,
      partial: false,
      forecast: true,
      basis: f.basis,
      samples: f.samples,
    }))
    return bucketize([...measured, ...predicted], MAX_COLS)
  }, [data])

  const maxCol = Math.max(1, ...cols.map((c) => c.credits))

  // ② 画像：按钟点 24 格 / 按星期几 168 格。
  const cells = useMemo(() => {
    const done = data.series.filter((s) => !s.partial)
    if (profMode === 'hour') {
      const acc = new Map<number, { ksk: number; credits: number; n: number }>()
      for (const s of done) {
        const e = acc.get(s.hour) ?? { ksk: 0, credits: 0, n: 0 }
        acc.set(s.hour, { ksk: e.ksk + s.ksk, credits: e.credits + s.credits, n: e.n + 1 })
      }
      return Array.from({ length: 24 }, (_, h) => {
        const e = acc.get(h)
        return {
          key: `${h}:00`,
          hour: h,
          ksk: e ? +(e.ksk / e.n).toFixed(1) : 0,
          credits: e ? +(e.credits / e.n).toFixed(1) : 0,
          samples: e?.n ?? 0,
          tick: h % 4 === 0 ? String(h) : '',
        }
      })
    }
    const acc = new Map<number, { ksk: number; credits: number; n: number }>()
    for (const s of done) {
      const k = s.weekday * 24 + s.hour
      const e = acc.get(k) ?? { ksk: 0, credits: 0, n: 0 }
      acc.set(k, { ksk: e.ksk + s.ksk, credits: e.credits + s.credits, n: e.n + 1 })
    }
    return Array.from({ length: 168 }, (_, k) => {
      const e = acc.get(k)
      const wd = Math.floor(k / 24)
      const hr = k % 24
      return {
        key: `周${WD[wd]} ${hr}:00`,
        hour: hr,
        ksk: e ? +(e.ksk / e.n).toFixed(1) : 0,
        credits: e ? +(e.credits / e.n).toFixed(1) : 0,
        samples: e?.n ?? 0,
        tick: hr === 12 ? WD[wd] : '',
      }
    })
  }, [data.series, profMode])

  const maxCell = Math.max(1, ...cells.map((c) => c.credits))
  const gaps = cells.filter((c) => c.samples === 0).length
  const step = Math.max(1, Math.ceil(cols.length / 6))

  return (
    <div className="space-y-6">
      {/* ── 每小时消耗 + 预测 ── */}
      <div>
        <p className="mb-2 text-sm font-medium">
          {t('restock.chart.hourly')}{' '}
          {data.forecast.length > 0 && (
            <span className="text-xs font-normal text-muted-foreground">
              {t('restock.chart.forecastTag')}
            </span>
          )}
        </p>
        {/* overflow-hidden 是必需的：几十根柱按 flex 分宽会有亚像素取整，
            累积出 1px 溢出就会长出横向滚动条。 */}
        <div className="flex h-32 items-end gap-[2px] overflow-hidden border-b pt-2">
          {cols.map((c, i) => {
            const other = Math.max(0, c.credits - c.ksk)
            const bought = data.buys.some((b) => b.ts >= c.ts && b.ts < c.tsEnd)
            return (
              <button
                key={c.ts}
                type="button"
                onClick={() =>
                  setPick(
                    c.forecast
                      ? `${fmtLocal(c.ts, off)}${c.span > 1 ? ` 起 ${c.span}h` : ''} · 预测 ksk_ ${c.ksk} 分 · 依据 ${c.basis ?? '—'}（${c.samples ?? 0} 样本）`
                      : `${fmtLocal(c.ts, off)}${c.span > 1 ? ` 起 ${c.span}h` : ''} · ksk_ ${c.ksk} 分 / ${c.ksk_calls} 次 · 全部 ${c.credits} 分 / ${c.calls} 次${c.partial ? '（当前小时未走完）' : ''}`,
                  )
                }
                className={`relative flex h-full min-w-0 flex-1 flex-col justify-end rounded-t-[3px] ${
                  c.forecast ? 'bg-amber-500/10' : ''
                } ${c.partial ? 'opacity-50' : ''}`}
                aria-label={fmtLocal(c.ts, off)}
              >
                {bought && (
                  <span className="absolute left-1/2 top-0.5 h-1.5 w-1.5 -translate-x-1/2 rounded-full bg-emerald-500 ring-1 ring-background" />
                )}
                {other > 0 && (
                  <span
                    className="w-full rounded-t-[2px] bg-muted-foreground/40"
                    style={{ height: `${(100 * other) / maxCol}%` }}
                  />
                )}
                {c.ksk > 0 && (
                  <span
                    className={`w-full rounded-t-[2px] ${
                      c.forecast
                        ? 'bg-[repeating-linear-gradient(45deg,var(--color-warn,#e8a33d),var(--color-warn,#e8a33d)_3px,transparent_3px,transparent_6px)]'
                        : 'bg-primary'
                    }`}
                    style={{ height: `${(100 * c.ksk) / maxCol}%` }}
                  />
                )}
                {i % step === 0 && <span className="sr-only">{fmtLocal(c.ts, off)}</span>}
              </button>
            )
          })}
        </div>
        <div className="mt-1 flex gap-[2px] overflow-hidden text-[10px] tabular-nums text-muted-foreground">
          {cols.map((c, i) => (
            <span key={c.ts} className="min-w-0 flex-1 truncate text-center">
              {i % step === 0 ? fmtLocal(c.ts, off).slice(6) : ''}
            </span>
          ))}
        </div>
        <p className="mt-2 min-h-[1.25rem] text-xs tabular-nums text-muted-foreground">
          {pick ?? `峰值 ${maxCol.toFixed(0)} 分 · 共 ${cols.length} 根${cols[0]?.span > 1 ? `（每根 ${cols[0].span} 小时）` : ''}`}
        </p>
      </div>

      {/* ── 画像 ── */}
      <div>
        <div className="mb-2 flex flex-wrap items-center justify-between gap-3">
          <p className="text-sm font-medium">{t('restock.chart.profile')}</p>
          <Segment
            options={[
              { value: 'hour', label: t('restock.chart.byHour') },
              { value: 'week', label: t('restock.chart.byWeekday') },
            ]}
            value={profMode}
            onChange={setProfMode}
          />
        </div>
        {/* 168 格窄屏放不下（167 个间隙就 334px），给它自己的横滚容器；
            页面整体仍然不横滚。 */}
        <div className="overflow-x-auto">
          <div style={cells.length > 40 ? { minWidth: `${cells.length * 6}px` } : undefined}>
            <div className="flex h-32 items-end gap-[2px] overflow-hidden border-b pt-2">
              {cells.map((c) => {
                const other = Math.max(0, c.credits - c.ksk)
                const on = inPeak(c.hour, data.peak_start, data.peak_end)
                return (
                  <button
                    key={c.key}
                    type="button"
                    onClick={() =>
                      setPick(
                        c.samples > 0
                          ? `${c.key} 平均 ${c.credits} 分（ksk_ ${c.ksk}，${c.samples} 样本）${on ? ' · 窗口内' : ' · 窗口外'}`
                          : `${c.key} 还没有样本`,
                      )
                    }
                    className={`flex h-full min-w-0 flex-1 flex-col justify-end rounded-t-[3px] ${on ? '' : 'opacity-40'}`}
                    aria-label={c.key}
                  >
                    {other > 0 && (
                      <span
                        className="w-full rounded-t-[2px] bg-muted-foreground/40"
                        style={{ height: `${(100 * other) / maxCell}%` }}
                      />
                    )}
                    {c.ksk > 0 && (
                      <span
                        className="w-full rounded-t-[2px] bg-primary"
                        style={{ height: `${(100 * c.ksk) / maxCell}%` }}
                      />
                    )}
                  </button>
                )
              })}
            </div>
            <div className="mt-1 flex gap-[2px] text-[10px] tabular-nums text-muted-foreground">
              {cells.map((c) => (
                <span key={c.key} className="min-w-0 flex-1 truncate text-center">
                  {c.tick}
                </span>
              ))}
            </div>
          </div>
        </div>
        <p className="mt-2 text-xs text-muted-foreground">
          {`${data.peak_start}–${data.peak_end}`}
          {cells.length > 40 ? ` · ${t('restock.chart.scrollHint')}` : ''}
          {gaps > 0 ? ` · ${gaps}/${cells.length} ${t('restock.chart.noSample')}` : ''}
        </p>
      </div>

      <div className="flex flex-wrap gap-4 text-xs text-muted-foreground">
        <span>
          <i className="mr-1.5 inline-block h-2.5 w-2.5 rounded-[3px] bg-primary align-[-1px]" />
          {t('restock.chart.legendKsk')}
        </span>
        <span>
          <i className="mr-1.5 inline-block h-2.5 w-2.5 rounded-[3px] bg-muted-foreground/40 align-[-1px]" />
          {t('restock.chart.legendOther')}
        </span>
        <span>
          <i className="mr-1.5 inline-block h-2.5 w-2.5 rounded-[3px] bg-amber-500 align-[-1px]" />
          {t('restock.chart.legendForecast')}
        </span>
        <span>
          <i className="mr-1.5 inline-block h-2.5 w-2.5 rounded-full bg-emerald-500 align-[-1px]" />
          {t('restock.chart.legendBuy')}
        </span>
      </div>

      {data.models.length > 0 && (
        <p className="text-xs leading-relaxed text-muted-foreground">
          <span className="font-medium">ksk_ 积分去向：</span>
          {data.models.map((m) => `${m.model} ${m.credits}`).join(' · ')}
        </p>
      )}
    </div>
  )
}
