import { useState } from 'react'
import {
  AlertTriangle,
  Coins,
  Loader2,
  PackageSearch,
  ShoppingCart,
  TrendingUp,
  Wallet,
} from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { Segment } from '@/components/ui/segment'
import { StatCard } from '@/components/ui/stat-card'
import { CreditCharts } from '@/features/restock/components/CreditCharts'
import {
  useBuyNow,
  useRestockAccounts,
  useRestockCredits,
  useRestockParams,
  useRestockState,
  useResetBreaker,
} from '@/features/restock/hooks'
import { useI18n } from '@/lib/i18n'

/** epoch → 本地 "MM-DD HH:MM"，按后端给的时区偏移换算（不用浏览器时区）。 */
function fmt(ts: number, offsetMinutes: number): string {
  const d = new Date((ts + offsetMinutes * 60) * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())} ${p(d.getUTCHours())}:${p(d.getUTCMinutes())}`
}

const ACTION_VARIANT = {
  buy: 'success',
  import: 'default',
  reclaim: 'warning',
  error: 'destructive',
  skip: 'muted',
} as const

export default function RestockPage() {
  const { t } = useI18n()
  const [hours, setHours] = useState(48)
  const { data: state, error: stateError } = useRestockState()
  const { data: credits, isLoading: creditsLoading } = useRestockCredits(hours)
  const { data: accounts } = useRestockAccounts()
  const { data: params } = useRestockParams()
  const buy = useBuyNow()
  const clearBreaker = useResetBreaker()

  const off = credits?.utc_offset_minutes ?? 480
  const snap = state?.snapshot ?? {}
  const banners: { tone: 'warn' | 'bad'; text: string }[] = []
  if (state) {
    if (!state.configured) banners.push({ tone: 'warn', text: t('restock.notConfigured') })
    if (state.dry_run) banners.push({ tone: 'warn', text: t('restock.banner.dryRun') })
    if (state.breaker)
      banners.push({ tone: 'bad', text: `${t('restock.banner.breaker')} ${state.breaker}` })
    if (state.orphan_orders > 0)
      banners.push({
        tone: 'bad',
        text: `${t('restock.banner.orphan')}（${state.orphan_orders}）`,
      })
    if (snap.any_online === false) banners.push({ tone: 'bad', text: t('restock.banner.offline') })
    if (!state.in_peak) banners.push({ tone: 'warn', text: t('restock.banner.outOfWindow') })
    if ((snap.zombie ?? 0) > 0)
      banners.push({ tone: 'warn', text: `${t('restock.banner.zombie')}（${snap.zombie}）` })
  }
  if (credits && !credits.coverage.mature)
    banners.push({
      tone: 'warn',
      text: `${t('restock.banner.coldStart')}（${credits.coverage.week_cells_ready}/168，已攒 ${credits.coverage.days_collected} 天）`,
    })

  return (
    <div className="space-y-6">
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <p className="eyebrow">Restock</p>
          <h1 className="mt-2 font-display text-4xl font-black tracking-[-0.04em]">
            {t('restock.title')}
          </h1>
          <p className="mt-2 text-sm text-muted-foreground">{t('restock.subtitle')}</p>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          <Badge variant={state?.enabled ? 'success' : 'muted'}>
            {state?.enabled ? t('restock.state.on') : t('restock.state.off')}
          </Badge>
          {state?.lease_holder && (
            <span className="text-xs text-muted-foreground">
              {t('restock.lease')}: <code>{state.lease_holder}</code>
            </span>
          )}
        </div>
      </header>

      {stateError && <ErrorNote error={stateError} />}
      {banners.map((b, i) => (
        <div
          key={i}
          className={`rounded-xl px-4 py-3 text-sm ${
            b.tone === 'bad'
              ? 'bg-destructive/10 text-destructive'
              : 'bg-amber-500/10 text-amber-700 dark:text-amber-300'
          }`}
        >
          {b.text}
        </div>
      ))}

      <div className="grid grid-cols-2 gap-4 md:grid-cols-3 xl:grid-cols-7">
        <StatCard
          icon={PackageSearch}
          label={t('restock.stat.healthy')}
          value={snap.healthy != null ? String(snap.healthy) : '—'}
          sub={
            `阈值 ${state?.min_healthy ?? '—'} · 冷却 ${snap.cooling ?? '—'}` +
            ((snap.zombie ?? 0) > 0 ? ` · 僵尸 ${snap.zombie}` : '')
          }
        />
        {/* 「这单划不划算」是买号策略的主判据：号按墙上时钟死，所以产出＝需求×寿命，
            需求不够时买号的单位成本会高过它要替代的贵号池。 */}
        <StatCard
          icon={Coins}
          label={t('restock.stat.unitCost')}
          value={
            snap.expected_unit_cost != null ? `¥${snap.expected_unit_cost.toFixed(3)}` : '∞'
          }
          sub={`${t('restock.stat.unitCostSub')} ¥${
            params?.values.max_unit_cost_cny_per_credit ?? '—'
          }`}
        />
        <StatCard
          icon={ShoppingCart}
          label={t('restock.stat.stock')}
          value={snap.stock != null ? String(snap.stock) : '—'}
        />
        <StatCard
          icon={Coins}
          label={t('restock.stat.price')}
          value={snap.price_usd != null ? `$${snap.price_usd.toFixed(2)}` : '—'}
        />
        <StatCard
          icon={Wallet}
          label={t('restock.stat.balance')}
          value={snap.balance_cny != null ? `¥${snap.balance_cny.toFixed(0)}` : '—'}
        />
        <StatCard
          icon={Wallet}
          label={t('restock.stat.spent')}
          value={state ? `¥${state.spent_today.toFixed(0)}` : '—'}
          sub={state ? `/ ¥${state.daily_cap_cny} · 已购 ${state.bought_today}` : undefined}
        />
        {/* 决策用的是「近期实测 与 预测下一小时 取大者」，所以这里显示的就是那个数，
            而不是图表里的窗口累计 —— 两个数不一致时看的人会以为面板在说谎。 */}
        <StatCard
          icon={TrendingUp}
          label={t('restock.stat.demand')}
          value={snap.demand_rate != null ? `${Math.round(snap.demand_rate)}` : '—'}
          sub={
            credits
              ? `分/时 · 未来 ${credits.forecast_hours}h 共 ${Math.round(credits.forecast_demand)} 分`
              : '分/时'
          }
        />
      </div>

      {snap.measured_lifetime_secs != null && (
        <p className="text-xs text-muted-foreground">
          {t('restock.lifetime.measured')}：
          <b>{Math.round(snap.measured_lifetime_secs / 60)} 分钟</b>
          （{snap.measured_lifetime_samples} 个样本） · 当前设定{' '}
          {Math.round((params?.values.expected_lifetime_secs ?? 0) / 60)} 分钟 ·{' '}
          {t('restock.lifetime.hint')}
        </p>
      )}

      <Card>
        <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-3">
          <CardTitle className="text-base">{t('restock.chart.hourly')}</CardTitle>
          <Segment
            options={[
              { value: 12, label: '12h' },
              { value: 48, label: '48h' },
              { value: 168, label: '7d' },
              { value: 720, label: '30d' },
            ]}
            value={hours}
            onChange={setHours}
          />
        </CardHeader>
        <CardContent>
          {creditsLoading && <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />}
          {credits && <CreditCharts data={credits} />}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">操作</CardTitle>
        </CardHeader>
        <CardContent className="flex flex-wrap items-center gap-3">
          <Button
            type="button"
            disabled={buy.isPending || !state?.configured}
            onClick={() => {
              if (window.confirm(t('restock.buyNowConfirm'))) buy.mutate()
            }}
          >
            {buy.isPending && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
            {t('restock.buyNow')}
          </Button>
          {state?.breaker && (
            <Button
              type="button"
              variant="destructive"
              disabled={clearBreaker.isPending}
              onClick={() => {
                if (window.confirm(t('restock.resetBreakerConfirm'))) clearBreaker.mutate()
              }}
            >
              <AlertTriangle className="mr-2 h-4 w-4" />
              {t('restock.resetBreaker')}
            </Button>
          )}
          {buy.data && <span className="text-sm text-muted-foreground">{buy.data.message}</span>}
          {buy.error && <ErrorNote error={buy.error} />}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('restock.decisions')}</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="max-h-96 space-y-2 overflow-y-auto">
            {(state?.decisions ?? []).map((d, i) => (
              <div key={`${d.ts}-${i}`} className="flex gap-3 border-b py-2 text-sm last:border-0">
                <time className="shrink-0 pt-0.5 text-xs tabular-nums text-muted-foreground">
                  {fmt(d.ts, off)}
                </time>
                <div className="min-w-0">
                  <Badge variant={ACTION_VARIANT[d.action] ?? 'muted'} className="mr-2">
                    {d.action}
                  </Badge>
                  <span className="break-words">{d.reason}</span>
                </div>
              </div>
            ))}
            {(state?.decisions ?? []).length === 0 && (
              <p className="text-sm text-muted-foreground">{t('restock.empty')}</p>
            )}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t('restock.accounts')}{' '}
            <span className="font-normal text-muted-foreground">({accounts?.count ?? 0})</span>
          </CardTitle>
        </CardHeader>
        <CardContent>
          <p className="mb-3 text-xs text-muted-foreground">{t('restock.acct.lifetime')}</p>
          <div className="max-h-[32rem] space-y-2 overflow-y-auto">
            {(accounts?.items ?? []).map((a) => (
              <div key={a.account_id} className="border-b py-2 text-sm last:border-0">
                <div className="flex flex-wrap items-center gap-2">
                  <code className="text-xs">{a.account_id}</code>
                  {a.self_bought && (
                    <Badge variant="success">{t('restock.acct.selfBought')}</Badge>
                  )}
                  {/* 死法要单列：「被风控封」和「key 被吊销」是两个完全不同的问题，
                      后者是供货质量，光看总产出分不出来。 */}
                  <Badge variant={a.reason ? 'warning' : 'success'}>
                    {a.reason || t('restock.acct.reasonAlive')}
                  </Badge>
                  {a.disabled && <Badge variant="destructive">disabled</Badge>}
                </div>
                <div className="mt-1 text-xs leading-relaxed text-muted-foreground">
                  {fmt(a.created_at, off)} 建 · 调用 {a.calls}（成功 {a.success}） · 积分{' '}
                  <b>{a.credits}</b>
                  {a.served_secs != null && (
                    <>
                      {' '}
                      · {t('restock.acct.served')} {Math.round(a.served_secs / 60)} 分钟
                    </>
                  )}
                  {a.cost_cny != null && (
                    <>
                      {' '}
                      · 成本 ¥{a.cost_cny.toFixed(2)}
                      {a.unit_cost_per_credit != null && (
                        <>
                          {' '}
                          · <b>¥{a.unit_cost_per_credit.toFixed(4)}</b>/
                          {t('restock.acct.perCredit')}
                        </>
                      )}
                      {a.unit_cost != null && (
                        <>
                          {' '}
                          · ¥{a.unit_cost.toFixed(4)}/{t('restock.acct.unitCost')}
                        </>
                      )}
                    </>
                  )}
                  <br />
                  并发 {a.max_concurrency}
                  {a.groups && ` · ${a.groups}`}
                </div>
              </div>
            ))}
            {(accounts?.items ?? []).length === 0 && (
              <p className="text-sm text-muted-foreground">{t('restock.empty')}</p>
            )}
          </div>
        </CardContent>
      </Card>
    </div>
  )
}
