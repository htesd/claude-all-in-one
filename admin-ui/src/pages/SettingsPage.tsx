import { useEffect, useRef, useState, type FormEvent } from 'react'
import { Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { RestockSettingsCard } from '@/features/restock/components/RestockSettingsCard'
import { WorkerEffectiveCard } from '@/features/settings/components/WorkerEffectiveCard'
import { useSettings, useUpdateSettings } from '@/features/settings/hooks'
import { useGroups } from '@/features/groups/hooks'
import type { SystemSettings, SystemSettingsPatch, ThinkingEffort } from '@/features/settings/types'
import { THINKING_EFFORTS } from '@/features/settings/types'
import { useI18n } from '@/lib/i18n'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

/** 一行分组暖机策略的表单态(数字以字符串存储,提交时解析)。 */
interface WarmupPolicyRow {
  group: string
  rpm: string
  hours: string
}

/** 表单内部状态：数字字段均以字符串存储，提交时解析。 */
interface FormState {
  default_proxy: string
  /** 出口池:文本框,每行一个代理 URL(提交时拆分成数组)。 */
  egress_pool: string
  cache_read_multiplier: string
  cache_cap_ratio: string
  cache_floor_ratio: string
  cache_sim_ttl_secs: string
  cache_max_sessions: string
  rate_limit_cooldown_secs: string
  suspended_cooldown_secs: string
  empty_response_cooldown_secs: string
  empty_response_window_secs: string
  empty_response_threshold: string
  affinity_ttl_secs: string
  max_failures: string
  max_switch_attempts: string
  /** RPM 闸等待预算(毫秒)。 */
  rpm_wait_ms: string
  quota_poll_enabled: boolean
  /** 低优先新号暖机(全局两期,按分组策略未覆盖的组走这里)。 */
  warmup_enabled: boolean
  warmup_phase1_hours: string
  warmup_phase1_rpm: string
  warmup_phase2_hours: string
  warmup_phase2_rpm: string
  /** 按分组的暖机策略编辑行。 */
  warmup_policies: WarmupPolicyRow[]
  image_enabled: boolean
  image_max_long_edge: string
  image_max_pixels_single: string
  image_max_pixels_multi: string
  image_multi_threshold: string
  tools_in_prefix: boolean
  cache_point: boolean
  agent_continuation: boolean
  thinking_signature: boolean
  q_endpoint: boolean
  default_thinking_effort: ThinkingEffort
}

function settingsToForm(s: SystemSettings): FormState {
  return {
    default_proxy: s.default_proxy ?? '',
    egress_pool: (s.egress_pool ?? []).join('\n'),
    cache_read_multiplier: String(s.cache_read_multiplier),
    cache_cap_ratio: String(s.cache_cap_ratio),
    cache_floor_ratio: String(s.cache_floor_ratio),
    cache_sim_ttl_secs: String(s.cache_sim_ttl_secs),
    cache_max_sessions: String(s.cache_max_sessions),
    rate_limit_cooldown_secs: String(s.rate_limit_cooldown_secs),
    suspended_cooldown_secs: String(s.suspended_cooldown_secs),
    empty_response_cooldown_secs: String(s.empty_response_cooldown_secs),
    empty_response_window_secs: String(s.empty_response_window_secs),
    empty_response_threshold: String(s.empty_response_threshold),
    affinity_ttl_secs: String(s.affinity_ttl_secs),
    max_failures: String(s.max_failures),
    max_switch_attempts: String(s.max_switch_attempts),
    // 以下字段旧版本后端可能不返回,与 default_thinking_effort 同口径地补基线默认。
    rpm_wait_ms: String(s.rpm_wait_ms ?? 10000),
    quota_poll_enabled: s.quota_poll_enabled,
    warmup_enabled: s.warmup_enabled ?? true,
    warmup_phase1_hours: String(s.warmup_phase1_hours ?? 2),
    warmup_phase1_rpm: String(s.warmup_phase1_rpm ?? 2),
    warmup_phase2_hours: String(s.warmup_phase2_hours ?? 24),
    warmup_phase2_rpm: String(s.warmup_phase2_rpm ?? 6),
    warmup_policies: Object.entries(s.warmup_group_policies ?? {}).map(([group, p]) => ({
      group,
      rpm: String(p.rpm),
      hours: String(p.hours),
    })),
    image_enabled: s.image_enabled,
    image_max_long_edge: String(s.image_max_long_edge),
    image_max_pixels_single: String(s.image_max_pixels_single),
    image_max_pixels_multi: String(s.image_max_pixels_multi),
    image_multi_threshold: String(s.image_multi_threshold),
    tools_in_prefix: s.tools_in_prefix,
    cache_point: s.cache_point,
    agent_continuation: s.agent_continuation,
    thinking_signature: s.thinking_signature ?? true,
    q_endpoint: s.q_endpoint ?? false,
    // 后端总会回灌该字段;真缺了(旧版本后端)退回 high,与后端基线一致。
    default_thinking_effort: s.default_thinking_effort ?? 'high',
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

  // egress_pool: 文本框(每行一个 URL)→ 数组;空 = 重置为 null。
  // 注:GET 返回的池 URL 密码段被掩码(***),未改动时下面比较为 false → 不回传掩码值。
  const poolLines = form.egress_pool
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l !== '')
  const serverPool = original.egress_pool ?? []
  const poolChanged =
    poolLines.length !== serverPool.length || poolLines.some((l, i) => l !== serverPool[i])
  if (poolChanged) {
    patch.egress_pool = poolLines.length === 0 ? null : poolLines
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
    ['suspended_cooldown_secs', 'suspended_cooldown_secs'],
    ['empty_response_cooldown_secs', 'empty_response_cooldown_secs'],
    ['empty_response_window_secs', 'empty_response_window_secs'],
    ['empty_response_threshold', 'empty_response_threshold'],
    ['affinity_ttl_secs', 'affinity_ttl_secs'],
    ['max_failures', 'max_failures'],
    ['max_switch_attempts', 'max_switch_attempts'],
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
    'thinking_signature',
    'q_endpoint',
  ]
  for (const k of boolFields) {
    if (form[k] !== original[k]) {
      ;(patch as Record<string, unknown>)[k] = form[k]
    }
  }

  // 新增字段(旧版本后端可能不返回):与「基线默认」比较,避免对旧后端捎带未知字段
  // (与 default_thinking_effort 的 ?? 兜底比较同口径)。
  // 解析用 Number+isInteger 而非 parseInt:'1e3' / '2.5' 这类输入不该被静默截断
  // (对抗审核 [中]);非法值=不提交该字段(与既有 NaN 跳过口径一致)。
  const strictInt = (s: string): number | null => {
    const n = Number(s)
    return s.trim() !== '' && Number.isInteger(n) && n >= 0 ? n : null
  }
  const newIntFields: Array<[keyof FormState, keyof SystemSettings, number]> = [
    ['rpm_wait_ms', 'rpm_wait_ms', 10000],
    ['warmup_phase1_hours', 'warmup_phase1_hours', 2],
    ['warmup_phase1_rpm', 'warmup_phase1_rpm', 2],
    ['warmup_phase2_hours', 'warmup_phase2_hours', 24],
    ['warmup_phase2_rpm', 'warmup_phase2_rpm', 6],
  ]
  for (const [fk, sk, def] of newIntFields) {
    const parsed = strictInt(form[fk] as string)
    if (parsed !== null && parsed !== ((original[sk] as number | undefined) ?? def)) {
      ;(patch as Record<string, unknown>)[sk] = parsed
    }
  }
  if (form.warmup_enabled !== (original.warmup_enabled ?? true)) {
    patch.warmup_enabled = form.warmup_enabled
  }

  // 按分组暖机策略:编辑行 → map(空组名/非法数字行丢弃;同组名后行覆盖前行,
  // 但 UI 已在下拉里排除其他行已用的组名,正常路径到不了覆盖)。
  const policyMap: Record<string, { rpm: number; hours: number }> = {}
  for (const row of form.warmup_policies) {
    const group = row.group.trim()
    const rpm = strictInt(row.rpm)
    const hours = strictInt(row.hours)
    if (group === '' || rpm === null || hours === null) continue
    policyMap[group] = { rpm, hours }
  }
  const serverPolicies = original.warmup_group_policies ?? {}
  const samePolicies =
    Object.keys(policyMap).length === Object.keys(serverPolicies).length &&
    Object.entries(policyMap).every(([g, p]) => {
      const sp = serverPolicies[g]
      return sp !== undefined && sp.rpm === p.rpm && sp.hours === p.hours
    })
  if (!samePolicies) {
    // 空表发 **显式空 map** 而不是 null:null 的语义是「删 overlay 回 YAML 基线」,
    // 若 YAML 配了非空策略,删完全部行保存后策略会复活(对抗审核 [中])。
    patch.warmup_group_policies = policyMap
  }

  // 枚举字段(档位):只在变了才回传。比较对象必须与 settingsToForm 同一口径地补默认值 ——
  // 直接跟原始响应比，遇到不返回该字段的旧后端就会 `'high' !== undefined` 恒成立，于是
  // 哪怕用户只改了代理也会捎带这个字段，被旧后端的 deny_unknown_fields 判 400，整个保存失败。
  if (form.default_thinking_effort !== (original.default_thinking_effort ?? 'high')) {
    patch.default_thinking_effort = form.default_thinking_effort
  }

  return patch
}

export default function SettingsPage() {
  const { t } = useI18n()
  const { data, isLoading, error: loadError } = useSettings()
  const { data: groups } = useGroups()
  const mutation = useUpdateSettings()

  const [form, setForm] = useState<FormState | null>(null)
  const [saved, setSaved] = useState(false)
  const [formError, setFormError] = useState<string | null>(null)
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

  // 分组暖机策略行的增删改(不走 set():它是数组不是标量)。
  const setPolicyRow = (idx: number, patch: Partial<WarmupPolicyRow>) => {
    setSaved(false)
    setForm((prev) =>
      prev === null
        ? prev
        : {
            ...prev,
            warmup_policies: prev.warmup_policies.map((r, i) =>
              i === idx ? { ...r, ...patch } : r,
            ),
          },
    )
  }
  const addPolicyRow = () => {
    setSaved(false)
    setForm((prev) =>
      prev === null
        ? prev
        : { ...prev, warmup_policies: [...prev.warmup_policies, { group: '', rpm: '2', hours: '24' }] },
    )
  }
  const removePolicyRow = (idx: number) => {
    setSaved(false)
    setForm((prev) =>
      prev === null
        ? prev
        : { ...prev, warmup_policies: prev.warmup_policies.filter((_, i) => i !== idx) },
    )
  }

  // 下拉选项 = 现有分组 ∪ 策略里已在用的组名(组被删后策略仍可辨认/移除)。
  const groupOptions = Array.from(
    new Set([...(groups ?? []).map((g) => g.name), ...(form?.warmup_policies ?? []).map((r) => r.group)]),
  ).filter((n) => n !== '')

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (!form || !data || mutation.isPending) return

    // 两期顺序守卫(对抗审核 [中]):phase2 < phase1 时 phase2 永远命不中
    // (号龄到 phase1 后直接毕业),存进去就是一份静默失效的配置。
    const p1 = Number(form.warmup_phase1_hours)
    const p2 = Number(form.warmup_phase2_hours)
    if (Number.isInteger(p1) && Number.isInteger(p2) && p2 < p1) {
      setFormError(t('settings.field.warmupPhaseOrderError'))
      return
    }
    setFormError(null)

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

      {/* 自动补货：自成一体的一段（自己的 GET/PUT、自己的保存按钮）。
          放在主 <form> 之外 —— 塞进去会被主表单的 submit 裹挟。 */}
      {/* 「我保存的到底生效没有」——放在改设置的同一页，不用切页也不用 SSH。 */}
      <WorkerEffectiveCard workers={data?.workers} desired={data} />

      <RestockSettingsCard />

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
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.egressPool')}
              </label>
              <textarea
                value={form?.egress_pool ?? ''}
                onChange={(e) => set('egress_pool', e.target.value)}
                placeholder={t('settings.field.egressPoolPlaceholder')}
                spellCheck={false}
                autoComplete="off"
                rows={3}
                disabled={isLoading || form === null}
                className={`${inputClass} font-mono`}
              />
              <p className="text-xs text-muted-foreground">{t('settings.field.egressPoolHint')}</p>
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
                {t('settings.field.suspendedCooldownSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.suspended_cooldown_secs ?? ''}
                onChange={(e) => set('suspended_cooldown_secs', e.target.value)}
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
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.maxSwitchAttempts')}
              </label>
              <input
                type="number"
                step={1}
                min={1}
                value={form?.max_switch_attempts ?? ''}
                onChange={(e) => set('max_switch_attempts', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
              <p className="text-xs text-muted-foreground">
                {t('settings.field.maxSwitchAttemptsHint')}
              </p>
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.rpmWaitMs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.rpm_wait_ms ?? ''}
                onChange={(e) => set('rpm_wait_ms', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
              <p className="text-xs text-muted-foreground">{t('settings.field.rpmWaitMsHint')}</p>
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

        {/* ── 新号暖机 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.warmup')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.warmup_enabled ?? true}
                onChange={(e) => set('warmup_enabled', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">{t('settings.field.warmupEnabled')}</span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.warmupEnabledHint')}
                </span>
              </span>
            </label>

            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.warmupPhase1Hours')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.warmup_phase1_hours ?? ''}
                  onChange={(e) => set('warmup_phase1_hours', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.warmupPhase1Rpm')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={1}
                  value={form?.warmup_phase1_rpm ?? ''}
                  onChange={(e) => set('warmup_phase1_rpm', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.warmupPhase2Hours')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.warmup_phase2_hours ?? ''}
                  onChange={(e) => set('warmup_phase2_hours', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.warmupPhase2Rpm')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={1}
                  value={form?.warmup_phase2_rpm ?? ''}
                  onChange={(e) => set('warmup_phase2_rpm', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
            </div>

            {/* 按分组策略:命中的组完全接管(单期);hours=0 = 该组关闭暖机 */}
            <div className="space-y-2">
              <span className="block text-sm font-medium">{t('settings.field.warmupPolicies')}</span>
              <p className="text-xs text-muted-foreground">
                {t('settings.field.warmupPoliciesHint')}
              </p>
              {(form?.warmup_policies ?? []).map((row, idx) => (
                <div key={idx} className="flex flex-wrap items-center gap-2">
                  <select
                    value={row.group}
                    onChange={(e) => setPolicyRow(idx, { group: e.target.value })}
                    disabled={isLoading || form === null}
                    className={`${inputClass} w-44`}
                  >
                    <option value="" disabled>
                      {t('settings.field.warmupPolicyGroupPlaceholder')}
                    </option>
                    {groupOptions
                      .filter(
                        (g) =>
                          g === row.group ||
                          !(form?.warmup_policies ?? []).some((r, i) => i !== idx && r.group === g),
                      )
                      .map((g) => (
                        <option key={g} value={g}>
                          {g}
                        </option>
                      ))}
                  </select>
                  <input
                    type="number"
                    step={1}
                    min={1}
                    value={row.rpm}
                    onChange={(e) => setPolicyRow(idx, { rpm: e.target.value })}
                    disabled={isLoading || form === null}
                    placeholder={t('settings.field.warmupPolicyRpmPlaceholder')}
                    className={`${inputClass} w-28`}
                  />
                  <input
                    type="number"
                    step={1}
                    min={0}
                    value={row.hours}
                    onChange={(e) => setPolicyRow(idx, { hours: e.target.value })}
                    disabled={isLoading || form === null}
                    placeholder={t('settings.field.warmupPolicyHoursPlaceholder')}
                    className={`${inputClass} w-32`}
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    onClick={() => removePolicyRow(idx)}
                    disabled={isLoading || form === null}
                  >
                    {t('settings.field.warmupPolicyRemove')}
                  </Button>
                </div>
              ))}
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={addPolicyRow}
                disabled={isLoading || form === null}
              >
                {t('settings.field.warmupPolicyAdd')}
              </Button>
            </div>
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

        {/* ── 思维链 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.thinking')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <div>
              <span className="mb-2 block text-sm font-medium">
                {t('settings.field.defaultThinkingEffort')}
              </span>
              <div className="inline-flex flex-wrap gap-1 rounded-2xl border bg-muted p-1">
                {THINKING_EFFORTS.map((level) => {
                  const active = (form?.default_thinking_effort ?? 'high') === level
                  return (
                    <button
                      key={level}
                      type="button"
                      disabled={isLoading || form === null}
                      onClick={() => set('default_thinking_effort', level)}
                      title={t(`settings.effort.${level}Hint`)}
                      className={`rounded-xl px-3 py-1.5 text-sm transition-colors disabled:opacity-50 ${
                        active
                          ? 'bg-background font-medium text-foreground shadow-sm'
                          : 'text-muted-foreground hover:text-foreground'
                      }`}
                    >
                      {level}
                    </button>
                  )
                })}
              </div>
              <p className="mt-2 text-xs text-muted-foreground">
                {t('settings.field.defaultThinkingEffortHint')}
              </p>
              <p className="mt-1 text-xs text-muted-foreground">
                {t(`settings.effort.${form?.default_thinking_effort ?? 'high'}Hint`)}
              </p>
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
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.thinking_signature ?? true}
                onChange={(e) => set('thinking_signature', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.thinkingSignature')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.thinkingSignatureHint')}
                </span>
              </span>
            </label>
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.q_endpoint ?? false}
                onChange={(e) => set('q_endpoint', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.qEndpoint')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.qEndpointHint')}
                </span>
              </span>
            </label>
          </CardContent>
        </Card>

        {/* 保存区 */}
        {mutation.isError && (
          <ErrorNote error={mutation.error} labelKey="common.actionFailed" />
        )}
        {formError !== null && (
          <p className="text-sm text-rose-600 dark:text-rose-300">{formError}</p>
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
