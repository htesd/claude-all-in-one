import { useEffect, useRef, useState, type FormEvent } from 'react'
import { Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { useSettings, useUpdateSettings } from '@/features/settings/hooks'
import type { SystemSettings, SystemSettingsPatch } from '@/features/settings/types'
import { useI18n } from '@/lib/i18n'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

/** 表单内部状态：数字字段均以字符串存储，提交时解析。 */
interface FormState {
  default_proxy: string
  cache_read_multiplier: string
  cache_cap_ratio: string
  cache_floor_ratio: string
  cache_sim_ttl_secs: string
  cache_max_sessions: string
  rate_limit_cooldown_secs: string
  empty_response_cooldown_secs: string
  empty_response_window_secs: string
  empty_response_threshold: string
  affinity_ttl_secs: string
  max_failures: string
  quota_poll_enabled: boolean
  image_enabled: boolean
  image_max_long_edge: string
  image_max_pixels_single: string
  image_max_pixels_multi: string
  image_multi_threshold: string
  tools_in_prefix: boolean
  cache_point: boolean
  agent_continuation: boolean
}

function settingsToForm(s: SystemSettings): FormState {
  return {
    default_proxy: s.default_proxy ?? '',
    cache_read_multiplier: String(s.cache_read_multiplier),
    cache_cap_ratio: String(s.cache_cap_ratio),
    cache_floor_ratio: String(s.cache_floor_ratio),
    cache_sim_ttl_secs: String(s.cache_sim_ttl_secs),
    cache_max_sessions: String(s.cache_max_sessions),
    rate_limit_cooldown_secs: String(s.rate_limit_cooldown_secs),
    empty_response_cooldown_secs: String(s.empty_response_cooldown_secs),
    empty_response_window_secs: String(s.empty_response_window_secs),
    empty_response_threshold: String(s.empty_response_threshold),
    affinity_ttl_secs: String(s.affinity_ttl_secs),
    max_failures: String(s.max_failures),
    quota_poll_enabled: s.quota_poll_enabled,
    image_enabled: s.image_enabled,
    image_max_long_edge: String(s.image_max_long_edge),
    image_max_pixels_single: String(s.image_max_pixels_single),
    image_max_pixels_multi: String(s.image_max_pixels_multi),
    image_multi_threshold: String(s.image_multi_threshold),
    tools_in_prefix: s.tools_in_prefix,
    cache_point: s.cache_point,
    agent_continuation: s.agent_continuation,
  }
}

/** 只返回与服务器当前值不同的字段（增量 PATCH）。 */
function buildPatch(form: FormState, original: SystemSettings): SystemSettingsPatch {
  const patch: SystemSettingsPatch = {}

  // default_proxy: 空字符串 = 重置为 null
  const proxyValue = form.default_proxy.trim()
  const serverProxy = original.default_proxy ?? ''
  if (proxyValue !== serverProxy) {
    patch.default_proxy = proxyValue === '' ? null : proxyValue
  }

  // 浮点数字段
  const floatFields: Array<[keyof FormState, keyof SystemSettings]> = [
    ['cache_read_multiplier', 'cache_read_multiplier'],
    ['cache_cap_ratio', 'cache_cap_ratio'],
    ['cache_floor_ratio', 'cache_floor_ratio'],
  ]
  for (const [fk, sk] of floatFields) {
    const parsed = parseFloat(form[fk] as string)
    if (!isNaN(parsed) && parsed !== (original[sk] as number)) {
      ;(patch as Record<string, unknown>)[sk] = parsed
    }
  }

  // 整数字段
  const intFields: Array<[keyof FormState, keyof SystemSettings]> = [
    ['cache_sim_ttl_secs', 'cache_sim_ttl_secs'],
    ['cache_max_sessions', 'cache_max_sessions'],
    ['rate_limit_cooldown_secs', 'rate_limit_cooldown_secs'],
    ['empty_response_cooldown_secs', 'empty_response_cooldown_secs'],
    ['empty_response_window_secs', 'empty_response_window_secs'],
    ['empty_response_threshold', 'empty_response_threshold'],
    ['affinity_ttl_secs', 'affinity_ttl_secs'],
    ['max_failures', 'max_failures'],
    ['image_max_long_edge', 'image_max_long_edge'],
    ['image_max_pixels_single', 'image_max_pixels_single'],
    ['image_max_pixels_multi', 'image_max_pixels_multi'],
    ['image_multi_threshold', 'image_multi_threshold'],
  ]
  for (const [fk, sk] of intFields) {
    const parsed = parseInt(form[fk] as string, 10)
    if (!isNaN(parsed) && parsed !== (original[sk] as number)) {
      ;(patch as Record<string, unknown>)[sk] = parsed
    }
  }

  // 布尔字段
  const boolFields: Array<keyof SystemSettings & keyof FormState> = [
    'quota_poll_enabled',
    'image_enabled',
    'tools_in_prefix',
    'cache_point',
    'agent_continuation',
  ]
  for (const k of boolFields) {
    if (form[k] !== original[k]) {
      ;(patch as Record<string, unknown>)[k] = form[k]
    }
  }

  return patch
}

export default function SettingsPage() {
  const { t } = useI18n()
  const { data, isLoading, error: loadError } = useSettings()
  const mutation = useUpdateSettings()

  const [form, setForm] = useState<FormState | null>(null)
  const [saved, setSaved] = useState(false)
  const savedTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  // 首次加载完成时初始化表单
  useEffect(() => {
    if (data !== undefined && form === null) {
      setForm(settingsToForm(data))
    }
  }, [data, form])

  const set = (key: keyof FormState, value: string | boolean) => {
    setSaved(false)
    setForm((prev) => (prev === null ? prev : { ...prev, [key]: value }))
  }

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (!form || !data || mutation.isPending) return

    const patch = buildPatch(form, data)
    if (Object.keys(patch).length === 0) {
      setSaved(true)
      return
    }

    mutation.mutate(patch, {
      onSuccess: (newData) => {
        setForm(settingsToForm(newData))
        setSaved(true)
        if (savedTimerRef.current !== null) clearTimeout(savedTimerRef.current)
        savedTimerRef.current = setTimeout(() => setSaved(false), 3000)
      },
    })
  }

  return (
    <div className="space-y-6">
      {/* 页头 */}
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <p className="eyebrow">Settings</p>
          <h1 className="mt-2 font-display text-4xl font-black tracking-[-0.04em]">{t('settings.title')}</h1>
          <p className="mt-2 text-sm text-muted-foreground">{t('settings.subtitle')}</p>
        </div>
      </header>

      {/* 静态提示：进程拓扑需重启 */}
      <div className="flex items-start gap-2 rounded-xl bg-muted/50 px-4 py-3 text-xs text-muted-foreground">
        <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
        <span>{t('settings.restartNote')}</span>
      </div>

      {/* 加载失败 */}
      {loadError !== null && loadError !== undefined && <ErrorNote error={loadError} />}

      <form onSubmit={handleSubmit} className="space-y-6">
        {/* ── 代理 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.proxy')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.defaultProxy')}
              </label>
              <input
                type="text"
                value={form?.default_proxy ?? ''}
                onChange={(e) => set('default_proxy', e.target.value)}
                placeholder={t('settings.field.defaultProxyPlaceholder')}
                spellCheck={false}
                autoComplete="off"
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
          </CardContent>
        </Card>

        {/* ── 缓存 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.cache')}</CardTitle>
          </CardHeader>
          <CardContent className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.cacheReadMultiplier')}
              </label>
              <input
                type="number"
                step="any"
                min={0}
                value={form?.cache_read_multiplier ?? ''}
                onChange={(e) => set('cache_read_multiplier', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.cacheCapRatio')}
              </label>
              <input
                type="number"
                step="any"
                min={0}
                value={form?.cache_cap_ratio ?? ''}
                onChange={(e) => set('cache_cap_ratio', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.cacheFloorRatio')}
              </label>
              <input
                type="number"
                step="any"
                min={0}
                value={form?.cache_floor_ratio ?? ''}
                onChange={(e) => set('cache_floor_ratio', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.cacheSimTtlSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.cache_sim_ttl_secs ?? ''}
                onChange={(e) => set('cache_sim_ttl_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.cacheMaxSessions')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.cache_max_sessions ?? ''}
                onChange={(e) => set('cache_max_sessions', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
          </CardContent>
        </Card>

        {/* ── 调度 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.scheduler')}</CardTitle>
          </CardHeader>
          <CardContent className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.rateLimitCooldownSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.rate_limit_cooldown_secs ?? ''}
                onChange={(e) => set('rate_limit_cooldown_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.emptyResponseCooldownSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.empty_response_cooldown_secs ?? ''}
                onChange={(e) => set('empty_response_cooldown_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.emptyResponseWindowSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.empty_response_window_secs ?? ''}
                onChange={(e) => set('empty_response_window_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.emptyResponseThreshold')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.empty_response_threshold ?? ''}
                onChange={(e) => set('empty_response_threshold', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.affinityTtlSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.affinity_ttl_secs ?? ''}
                onChange={(e) => set('affinity_ttl_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.maxFailures')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.max_failures ?? ''}
                onChange={(e) => set('max_failures', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <label className="flex cursor-pointer items-center gap-3 sm:col-span-2 lg:col-span-3">
              <input
                type="checkbox"
                checked={form?.quota_poll_enabled ?? false}
                onChange={(e) => set('quota_poll_enabled', e.target.checked)}
                disabled={isLoading || form === null}
                className="h-4 w-4 rounded"
              />
              <span className="text-sm font-medium">{t('settings.field.quotaPollEnabled')}</span>
            </label>
          </CardContent>
        </Card>

        {/* ── 图像压缩 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.image')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            {/* 启用开关 */}
            <label className="flex cursor-pointer items-center gap-3">
              <input
                type="checkbox"
                checked={form?.image_enabled ?? false}
                onChange={(e) => set('image_enabled', e.target.checked)}
                disabled={isLoading || form === null}
                className="h-4 w-4 rounded"
              />
              <span className="text-sm font-medium">{t('settings.field.imageEnabled')}</span>
            </label>

            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.imageMaxLongEdge')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.image_max_long_edge ?? ''}
                  onChange={(e) => set('image_max_long_edge', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.imageMaxPixelsSingle')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.image_max_pixels_single ?? ''}
                  onChange={(e) => set('image_max_pixels_single', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.imageMaxPixelsMulti')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.image_max_pixels_multi ?? ''}
                  onChange={(e) => set('image_max_pixels_multi', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.imageMultiThreshold')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.image_multi_threshold ?? ''}
                  onChange={(e) => set('image_multi_threshold', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
            </div>
          </CardContent>
        </Card>

        {/* ── 实验性 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.experimental')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.tools_in_prefix ?? false}
                onChange={(e) => set('tools_in_prefix', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.toolsInPrefix')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.toolsInPrefixHint')}
                </span>
              </span>
            </label>
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.cache_point ?? false}
                onChange={(e) => set('cache_point', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.cachePoint')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.cachePointHint')}
                </span>
              </span>
            </label>
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.agent_continuation ?? false}
                onChange={(e) => set('agent_continuation', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.agentContinuation')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.agentContinuationHint')}
                </span>
              </span>
            </label>
          </CardContent>
        </Card>

        {/* 保存区 */}
        {mutation.isError && (
          <ErrorNote error={mutation.error} labelKey="common.actionFailed" />
        )}

        <div className="flex items-center justify-end gap-3">
          {saved && (
            <span className="text-sm text-emerald-700 dark:text-emerald-300">{t('settings.saved')}</span>
          )}
          <Button type="submit" disabled={isLoading || form === null || mutation.isPending}>
            {mutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
            {mutation.isPending ? t('settings.saving') : t('settings.save')}
          </Button>
        </div>
      </form>
    </div>
  )
}
