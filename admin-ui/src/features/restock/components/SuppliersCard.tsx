import { useEffect, useState } from 'react'
import { Loader2, Plus, Trash2 } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'

import { useSuppliers, useUpdateSuppliers } from '../hooks'
import type { ShelfView, SupplierConfig, SupplierPatch, SupplierView } from '../types'

const inputClass =
  'w-full rounded-xl border bg-input px-3 py-2 text-sm text-foreground outline-none ' +
  'transition-colors placeholder:text-muted-foreground'

/** 行内小标签，比 Badge 更轻，用于货架这种密集信息。 */
function Chip({ tone, children }: { tone: 'good' | 'warn' | 'bad' | 'mute'; children: React.ReactNode }) {
  const cls = {
    good: 'bg-emerald-500/12 text-emerald-700 dark:text-emerald-300',
    warn: 'bg-amber-500/12 text-amber-700 dark:text-amber-300',
    bad: 'bg-destructive/12 text-destructive',
    mute: 'bg-muted text-muted-foreground',
  }[tone]
  return <span className={`rounded-md px-1.5 py-0.5 text-[11px] ${cls}`}>{children}</span>
}

/**
 * 这一家可以配档位的货架键：**实时报价里出现过的** ∪ **名册里已经配过的**。
 *
 * 并上已配过的键是为了让写错的键**显示出来**而不是被悄悄丢掉 ——
 * 静默丢弃用户数据比留着一个标红的坏键危险得多。
 */
function shelfKeys(c: SupplierPatch, views: SupplierView[]): string[] {
  const live = (views.find((v) => v.id === c.id)?.shelves ?? []).map((s) => s.shelf)
  return [...new Set([...live, ...Object.keys(c.shelf_priority)])].sort()
}

/**
 * 货源面板：**每一家**的余额、货架、状态、今日花费 + 名册编辑。
 *
 * 为什么必须逐家列而不是像以前那样只显示一个「余额」：多供应商之后
 * 「还剩多少额度」不再是一个数。只显示一家的后果是 —— 那家余额充足、
 * 另一家钱包见底导致整轮买不到号，而面板上一切正常。
 *
 * **展示顺序**复刻 `supplier::rank_shelves`（档位 → 单价 → id 定序），
 * 但「下一单会买哪个」**不自己算** —— 那个答案由后端的 `choose_shelf` 给出
 * （`snap.next_pick`），因为它还要过余额、单价上限、日上限、`unit_cost_veto`
 * 这些前端拿不到的闸门。面板说会买 A 而引擎买了 B，是最难查的一类 bug：
 * 人只会怀疑引擎坏了，不会想到是面板在骗自己。
 */
export function SuppliersCard({
  views,
  nextPick,
  nextPickWhy,
}: {
  views: SupplierView[]
  nextPick?: string | null
  nextPickWhy?: string | null
}) {
  const { data, error } = useSuppliers()
  const mutation = useUpdateSuppliers()
  const [form, setForm] = useState<SupplierPatch[] | null>(null)
  const [saved, setSaved] = useState(false)

  useEffect(() => {
    if (!data) return
    setForm(
      data.items.map((c: SupplierConfig) => ({
        id: c.id,
        kind: c.kind,
        enabled: c.enabled,
        base_url: c.base_url,
        daily_cap_cny: c.daily_cap_cny,
        priority: c.priority ?? 0,
        shelf_priority: { ...(c.shelf_priority ?? {}) },
        // 不传 = 保留原密钥。用户不动这一格时绝不能把空串存回去。
        api_key: undefined,
      })),
    )
  }, [data])

  const set = (i: number, patch: Partial<SupplierPatch>) => {
    setForm((f) => (f ? f.map((x, j) => (j === i ? { ...x, ...patch } : x)) : f))
    setSaved(false)
  }

  // ── 展示顺序：与 `supplier::rank_shelves` 同一把尺子 ──
  // `unit_price_cny > 0` 这道过滤不是装饰：上游报价抖动解析出 0 时，引擎会剔除该货架
  // （有专门的单测钉住），面板漏了这道就会把「¥0.00 的最优货架」排到第一。
  const tier = (s: ShelfView) => s.priority ?? 0
  const live = (v: SupplierView) => v.shelves.filter((s) => s.stock > 0 && s.unit_price_cny > 0)
  const byRank = (a: ShelfView, b: ShelfView) =>
    tier(a) - tier(b) || a.unit_price_cny - b.unit_price_cny || a.label.localeCompare(b.label)
  const rank = (v: SupplierView): [number, number] => {
    const ls = [...live(v)].sort(byRank)
    // 一家有多个货架时，代表这家的是它**最优的那个**。
    return ls.length ? [tier(ls[0]), ls[0].unit_price_cny] : [Infinity, Infinity]
  }
  const sorted = [...views].sort((a, b) => {
    const [at, ap] = rank(a)
    const [bt, bp] = rank(b)
    return at - bt || ap - bp || a.id.localeCompare(b.id)
  })
  // 「下一单」精确到**货架**：一家有 us / eu 两个货架且档位不同时，只说「从 kiroapp 买」
  // 等于没说 —— 而那正是这次改动要区分的两批号。答案来自后端，不在这里算。
  const winnerId = nextPick ? nextPick.split('/')[0] : undefined

  return (
    <Card>
      <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
        <CardTitle className="text-base">货源</CardTitle>
        <span className="text-xs text-muted-foreground">
          规则：<b>档位小的优先，同档比价</b>
          {nextPick ? (
            <> · 下一单会买 <b>{nextPick}</b></>
          ) : (
            /* 买不成时把逐货架被否的理由显示出来 —— 只说「暂时不买」等于让人去翻日志。 */
            nextPickWhy && <> · 此刻买不成：{nextPickWhy}</>
          )}
        </span>
      </CardHeader>
      <CardContent className="space-y-4">
        {error && <ErrorNote error={error} />}
        {sorted.length === 0 && (
          <p className="text-sm text-muted-foreground">还没有货源报价（首轮询价前是空的）。</p>
        )}

        {/* 逐家一张小卡。窄屏一列、宽屏两列 —— 用户常在手机上看。 */}
        <div className="grid gap-3 lg:grid-cols-2">
          {sorted.map((v) => {
            const cfg = data?.items.find((c) => c.id === v.id)
            const capped = v.daily_cap_cny > 0
            const pct = capped ? Math.min(100, (v.spent_today_cny / v.daily_cap_cny) * 100) : 0
            return (
              <div key={v.id} className="rounded-2xl border p-3">
                <div className="flex flex-wrap items-center gap-2">
                  <b className="text-sm">{v.id}</b>
                  <Badge variant="muted">{v.kind}</Badge>
                  {!v.enabled && <Badge variant="muted">已停用</Badge>}
                  {v.enabled && !v.configured && <Badge variant="destructive">缺密钥</Badge>}
                  {v.blocked && <Badge variant="destructive">{v.blocked}</Badge>}
                  {v.error && <Badge variant="warning">询价失败</Badge>}
                  {winnerId === v.id && <Badge variant="success">下一单</Badge>}
                </div>

                {v.error && (
                  <p className="mt-1 break-words text-xs text-amber-700 dark:text-amber-300">
                    {v.error}
                  </p>
                )}

                <div className="mt-2 text-xs text-muted-foreground">
                  余额{' '}
                  <b className="text-foreground">
                    {v.balance_cny != null ? `¥${v.balance_cny.toFixed(2)}` : '—'}
                  </b>
                  {/* 原生单位一起显示：只给折算后的 ¥，人对不上对方网站的数字，
                      而对不上账的第一反应永远是「系统算错了」。 */}
                  {v.balance_native && <>（{v.balance_native}）</>}
                  {' · 今日已花 '}
                  <b className="text-foreground">¥{v.spent_today_cny.toFixed(2)}</b>
                  {capped ? ` / ¥${v.daily_cap_cny.toFixed(0)}` : '（本家不限）'}
                </div>
                {capped && (
                  <div className="mt-1.5 h-1 w-full overflow-hidden rounded-full bg-muted">
                    <div
                      className={`h-full ${pct >= 100 ? 'bg-destructive' : 'bg-primary'}`}
                      style={{ width: `${pct}%` }}
                    />
                  </div>
                )}

                {/* 货架：多供应商的库存是**逐货架**的，合成一个总数会掩盖
                    「有货但都在贵的那个区」这种最需要看见的情况。
                    每个货架都带**生效档位** —— `shelf_priority` 的键写错不会报错，
                    只会静默回落本家档位，这个数字是唯一能让写错当场看见的地方。 */}
                <div className="mt-2 flex flex-wrap gap-1.5">
                  {v.shelves.length === 0 && <Chip tone="mute">无货架数据</Chip>}
                  {[...v.shelves]
                    .sort(byRank)
                    .map((s) => (
                      <Chip
                        key={s.label}
                        tone={nextPick === s.label ? 'good' : s.stock > 0 ? 'warn' : 'mute'}
                      >
                        <span className="opacity-70">档{tier(s)}</span>{' '}
                        {s.shelf || '默认'} ¥{s.unit_price_cny.toFixed(2)} × {s.stock}
                        {s.region && ` · ${s.region}`}
                      </Chip>
                    ))}
                </div>

                {cfg?.breaker && (
                  <p className="mt-2 break-words rounded-lg bg-destructive/10 px-2 py-1 text-xs text-destructive">
                    熔断：{cfg.breaker}
                  </p>
                )}
              </div>
            )
          })}
        </div>

        {/* ── 名册编辑 ── */}
        {form && (
          <div className="space-y-3 border-t pt-4">
            <p className="text-xs font-medium text-muted-foreground">
              名册（保存后<b>下一轮补货自动生效，不用重启</b>）
            </p>
            {/* 窄屏堆叠、宽屏一行；表体自带横向滚动，页面本身不会横向滚。 */}
            <div className="space-y-3">
              {form.map((c, i) => {
                const cfg = data?.items.find((x) => x.id === c.id)
                return (
                  <div
                    key={i}
                    className="grid gap-2 rounded-2xl border p-3 sm:grid-cols-2 lg:grid-cols-[1fr_1fr_1.4fr_.8fr_.8fr_auto]"
                  >
                    <div className="space-y-1">
                      <label className="text-[11px] text-muted-foreground">
                        标识（改名等于换一家）
                      </label>
                      <input
                        className={inputClass}
                        value={c.id}
                        onChange={(e) => set(i, { id: e.target.value })}
                      />
                    </div>
                    <div className="space-y-1">
                      <label className="text-[11px] text-muted-foreground">类型</label>
                      <select
                        className={inputClass}
                        value={c.kind}
                        onChange={(e) => set(i, { kind: e.target.value })}
                      >
                        <option value="drop">drop</option>
                        <option value="kiroapp">kiroapp</option>
                      </select>
                    </div>
                    <div className="space-y-1">
                      <label className="text-[11px] text-muted-foreground">
                        API Key（{cfg?.has_key ? '已设置，留空不改' : '未设置'}）
                      </label>
                      <input
                        className={inputClass}
                        type="password"
                        autoComplete="new-password"
                        placeholder={cfg?.has_key ? '••••••（留空保持原值）' : '必填'}
                        value={c.api_key ?? ''}
                        onChange={(e) =>
                          set(i, { api_key: e.target.value === '' ? undefined : e.target.value })
                        }
                      />
                    </div>
                    <div className="space-y-1">
                      <label className="text-[11px] text-muted-foreground">
                        本家日上限 ¥（0=不限）
                      </label>
                      <input
                        className={inputClass}
                        type="number"
                        min={0}
                        step={1}
                        value={String(c.daily_cap_cny)}
                        onChange={(e) => set(i, { daily_cap_cny: Number(e.target.value) })}
                      />
                    </div>
                    <div className="space-y-1">
                      <label className="text-[11px] text-muted-foreground">档位（小=优先）</label>
                      <input
                        className={inputClass}
                        type="number"
                        step={1}
                        value={String(c.priority)}
                        onChange={(e) => set(i, { priority: Math.trunc(Number(e.target.value)) || 0 })}
                      />
                    </div>
                    <div className="flex items-end gap-2 pb-0.5">
                      <label className="flex cursor-pointer items-center gap-1.5 text-xs">
                        <input
                          type="checkbox"
                          className="h-4 w-4 rounded"
                          checked={c.enabled}
                          onChange={(e) => set(i, { enabled: e.target.checked })}
                        />
                        启用
                      </label>
                      <Button
                        type="button"
                        variant="ghost"
                        size="sm"
                        title="删除这一家"
                        onClick={() => {
                          setForm((f) => (f ? f.filter((_, j) => j !== i) : f))
                          setSaved(false)
                        }}
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>

                    {/* ── 逐货架档位覆盖 ──
                        必须细到货架：实测出问题的是 kiroapp/eu 这一个货架，而
                        kiroapp/us 是同一家的另一批号。只能整家降级的话，
                        要么误伤 US、要么放过 EU。

                        货架名从**实时报价**里取而不是让人手输 —— 手输一个
                        对不上的键是静默失效，是这套配置唯一的坑。 */}
                    {shelfKeys(c, views).length > 0 && (
                      <div className="space-y-1 sm:col-span-2 lg:col-span-6">
                        <label className="text-[11px] text-muted-foreground">
                          逐货架档位（留空=跟随本家档位 {c.priority}）
                        </label>
                        <div className="flex flex-wrap gap-2">
                          {shelfKeys(c, views).map((k) => {
                            const known = (views.find((v) => v.id === c.id)?.shelves ?? []).some(
                              (s) => s.shelf === k,
                            )
                            return (
                              <label key={k} className="flex items-center gap-1.5 text-xs">
                                <span className={known ? '' : 'text-destructive'}>
                                  {k || '默认'}
                                  {!known && '（无此货架）'}
                                </span>
                                <input
                                  className={`${inputClass} w-20`}
                                  type="number"
                                  step={1}
                                  placeholder={String(c.priority)}
                                  value={c.shelf_priority[k] ?? ''}
                                  onChange={(e) => {
                                    const next = { ...c.shelf_priority }
                                    if (e.target.value === '') delete next[k]
                                    else next[k] = Math.trunc(Number(e.target.value)) || 0
                                    set(i, { shelf_priority: next })
                                  }}
                                />
                              </label>
                            )
                          })}
                        </div>
                      </div>
                    )}
                  </div>
                )
              })}
            </div>

            <div className="flex flex-wrap items-center gap-3">
              <Button
                type="button"
                variant="outline"
                onClick={() => {
                  setForm((f) =>
                    f
                      ? [
                          ...f,
                          {
                            id: '',
                            kind: 'kiroapp',
                            enabled: true,
                            base_url: '',
                            daily_cap_cny: 0,
                            priority: 0,
                            shelf_priority: {},
                          },
                        ]
                      : f,
                  )
                  setSaved(false)
                }}
              >
                <Plus className="mr-1 h-4 w-4" />
                加一家
              </Button>
              <Button
                type="button"
                disabled={mutation.isPending}
                onClick={() => {
                  // 停用一家 = 从此不再从它买。值得一次确认：断供时误停会直接掉到贵号池。
                  const off = form.filter((c) => !c.enabled).map((c) => c.id)
                  if (
                    off.length > 0 &&
                    !window.confirm(`确认停用 ${off.join('、')}？停用后不再从它补货。`)
                  )
                    return
                  mutation.mutate(form, { onSuccess: () => setSaved(true) })
                }}
              >
                {mutation.isPending && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                保存名册
              </Button>
              {saved && <span className="text-xs text-muted-foreground">已保存</span>}
              {mutation.error && <ErrorNote error={mutation.error} />}
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  )
}
