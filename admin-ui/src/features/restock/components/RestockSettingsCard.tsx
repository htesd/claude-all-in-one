import { useEffect, useState } from 'react'
import { Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { useI18n } from '@/lib/i18n'

import { useRestockParams, useUpdateRestockParams } from '../hooks'
import type { ParamBound, RestockParams } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

/**
 * 系统设置里的「自动补货」段：开关 + 高峰窗口 + 全部可调参数。
 *
 * **自成一体，不并进 SettingsPage 的主表单** —— 补货参数存在 control.db 自己的键里，
 * 走 `/restock/params`，而主表单走 `/settings`（SystemSettings）。后者有一条回滚地板：
 * 给它加字段会让回滚到 2026-07-31 之前的镜像变成全量 503。两套状态各管各的最干净。
 */
export function RestockSettingsCard() {
  const { t } = useI18n()
  const { data, isLoading, error } = useRestockParams()
  const mutation = useUpdateRestockParams()
  const [form, setForm] = useState<Record<string, string | boolean> | null>(null)
  const [saved, setSaved] = useState(false)

  useEffect(() => {
    if (!data) return
    const next: Record<string, string | boolean> = {}
    for (const b of data.spec) {
      const v = data.values[b.key]
      next[b.key] = b.kind === 'bool' ? Boolean(v) : String(v ?? '')
    }
    setForm(next)
  }, [data])

  if (error) return <ErrorNote error={error} />

  const set = (key: string, value: string | boolean) => {
    setForm((f) => (f ? { ...f, [key]: value } : f))
    setSaved(false)
  }

  const handleSave = () => {
    if (!form || !data) return
    const patch: Record<string, unknown> = {}
    for (const b of data.spec) {
      const raw = form[b.key]
      const cur = data.values[b.key]
      if (b.kind === 'bool') {
        if (Boolean(raw) !== Boolean(cur)) patch[b.key] = Boolean(raw)
      } else if (b.kind === 'hhmm' || b.kind === 'url') {
        // url 要 trim：粘贴 webhook 时带上尾随空格很常见，而后端只认严格前缀。
        const next = b.kind === 'url' ? String(raw).trim() : String(raw)
        if (next !== String(cur)) patch[b.key] = next
      } else {
        const n = Number(raw)
        if (Number.isFinite(n) && n !== Number(cur)) patch[b.key] = n
      }
    }
    if (Object.keys(patch).length === 0) {
      setSaved(true)
      return
    }
    // 打开总开关 = 从此会真的扣款，值得一次确认。
    if (patch.enabled === true && !window.confirm(t('restock.confirmEnable'))) return
    if (patch.dry_run === false && !window.confirm(t('restock.confirmLive'))) return
    mutation.mutate(patch, { onSuccess: () => setSaved(true) })
  }

  const bounds = data?.spec ?? []
  const bool = (b: ParamBound) => (
    <label key={b.key} className="flex cursor-pointer items-start gap-3">
      <input
        type="checkbox"
        className="mt-1 h-4 w-4 rounded"
        checked={Boolean(form?.[b.key])}
        onChange={(e) => set(b.key, e.target.checked)}
        disabled={isLoading || form === null}
      />
      <span>
        <span className="block text-sm font-medium">{b.label}</span>
        <span className="block text-xs text-muted-foreground">{b.hint}</span>
      </span>
    </label>
  )

  const field = (b: ParamBound) => (
    // webhook 地址独占一整行：挤在三列数字里只能看见开头十几个字符，
    // 而「填错了」恰恰只在末尾（key 少一位）看得出来。
    <div key={b.key} className={`space-y-1.5 ${b.kind === 'url' ? 'sm:col-span-2 lg:col-span-3' : ''}`}>
      <label className="text-xs font-medium text-muted-foreground">{b.label}</label>
      {b.kind === 'url' ? (
        <input
          type="url"
          inputMode="url"
          placeholder="https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=..."
          value={String(form?.[b.key] ?? '')}
          onChange={(e) => set(b.key, e.target.value)}
          disabled={isLoading || form === null}
          className={inputClass}
        />
      ) : b.kind === 'hhmm' ? (
        <input
          type="time"
          value={String(form?.[b.key] ?? '')}
          onChange={(e) => set(b.key, e.target.value)}
          disabled={isLoading || form === null}
          className={inputClass}
        />
      ) : (
        <input
          type="number"
          step={b.kind === 'int' ? 1 : 0.01}
          min={b.min}
          max={b.max}
          value={String(form?.[b.key] ?? '')}
          onChange={(e) => set(b.key, e.target.value)}
          disabled={isLoading || form === null}
          className={inputClass}
        />
      )}
      <p className="text-xs text-muted-foreground">{b.hint}</p>
    </div>
  )

  const byKey = (k: keyof RestockParams) => bounds.find((b) => b.key === k)
  const switches = ['enabled', 'dry_run', 'new_account_queue_enabled'] as const
  const windowKeys = ['peak_start', 'peak_end', 'utc_offset_minutes'] as const
  const rest = bounds.filter(
    (b) =>
      !switches.includes(b.key as (typeof switches)[number]) &&
      !windowKeys.includes(b.key as (typeof windowKeys)[number]),
  )

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('settings.section.restock')}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        {data && !data.configured && (
          <div className="rounded-xl bg-muted/50 px-4 py-3 text-xs text-muted-foreground">
            {t('restock.notConfigured')}
          </div>
        )}

        {/* 开关：用户最常来这里就是为了这一个。 */}
        <div className="space-y-3">
          {switches.map((k) => {
            const b = byKey(k)
            return b ? bool(b) : null
          })}
        </div>

        {/* 补号时间段 —— 用户明确要求放在系统设置里。 */}
        <div>
          <p className="mb-2 text-xs font-medium text-muted-foreground">
            {t('restock.windowTitle')}
          </p>
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {windowKeys.map((k) => {
              const b = byKey(k)
              return b ? field(b) : null
            })}
          </div>
        </div>

        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">{rest.map(field)}</div>

        <div className="flex items-center gap-3">
          <Button type="button" onClick={handleSave} disabled={mutation.isPending || form === null}>
            {mutation.isPending && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
            {t('settings.save')}
          </Button>
          {saved && <span className="text-xs text-muted-foreground">{t('settings.saved')}</span>}
          {mutation.error && <ErrorNote error={mutation.error} />}
        </div>
        <p className="text-xs text-muted-foreground">{t('restock.hotNote')}</p>
      </CardContent>
    </Card>
  )
}
