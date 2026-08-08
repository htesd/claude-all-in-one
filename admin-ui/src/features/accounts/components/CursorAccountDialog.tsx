import { useEffect, useState } from 'react'
import { ChevronRight, Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Segment } from '@/components/ui/segment'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { useSettings } from '@/features/settings/hooks'
import { extractErrorMessage, getErrorStatus } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { useCreateAccount } from '../hooks'
import { buildCursorExtra, parseConcurrency, tierToPriority, type PriorityTier } from '../lib'
import { ACCOUNT_ID_PATTERN } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm outline-none transition-colors placeholder:text-muted-foreground'

function gatewayLabel(url: string, i: number): string {
  let host = url
  try {
    host = new URL(url).host || url
  } catch {
    host = url
  }
  return `${i + 1}. ${host}`
}

interface CursorAccountDialogProps {
  open: boolean
  onClose: () => void
}

/**
 * Cursor 账号建号对话框（单步）：填凭据与可选字段 → `POST /accounts`（provider: 'cursor'）。
 *
 * 凭据来自真机 Cursor 客户端的 state.vscdb（cursorAuth/accessToken、cursorAuth/refreshToken）。
 * access_token 与 refresh_token 在 Cursor 侧本来就是同一个 JWT，两个值相同是正常的，
 * 这里**不去重也不校验一致性**（后端刷新时新 access_token 同时兼任新 refresh_token）。
 *
 * 设备指纹 / config_version / 时区 / 代理全部可留空（后端派生或走自动分配），
 * 故收进默认折叠的高级区；出口代理由后端自动分配，前端不做分配逻辑。
 */
export function CursorAccountDialog({ open, onClose }: CursorAccountDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const settingsQuery = useSettings()
  const gateways = settingsQuery.data?.egress_pool ?? []
  const createMutation = useCreateAccount()

  const [accountId, setAccountId] = useState('')
  const [group, setGroup] = useState('')
  const [concurrency, setConcurrency] = useState('1')
  const [priorityTier, setPriorityTier] = useState<PriorityTier>('low')
  const [egress, setEgress] = useState('auto')
  const [accessToken, setAccessToken] = useState('')
  const [refreshToken, setRefreshToken] = useState('')
  const [advancedOpen, setAdvancedOpen] = useState(false)
  const [machineId, setMachineId] = useState('')
  const [macMachineId, setMacMachineId] = useState('')
  const [configVersion, setConfigVersion] = useState('')
  const [timezone, setTimezone] = useState('')
  const [proxy, setProxy] = useState('')
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (open) {
      setAccountId('')
      setGroup('')
      setConcurrency('1')
      setPriorityTier('low')
      setEgress('auto')
      setAccessToken('')
      setRefreshToken('')
      setAdvancedOpen(false)
      setMachineId('')
      setMacMachineId('')
      setConfigVersion('')
      setTimezone('')
      setProxy('')
      setError(null)
    }
  }, [open])

  const handleSubmit = () => {
    if (createMutation.isPending) return
    if (!ACCOUNT_ID_PATTERN.test(accountId)) {
      setError(t('accounts.error.invalidId'))
      return
    }
    const parsedConcurrency = parseConcurrency(concurrency)
    if (parsedConcurrency === null) {
      setError(t('accounts.error.invalidConcurrency'))
      return
    }
    if (accessToken.trim() === '') {
      setError(t('accounts.cursor.error.tokenRequired'))
      return
    }
    setError(null)

    // extra 按后端 CURSOR_ACCOUNT_SCHEMA 组：空字符串一律不传（留空 = 后端派生/自动分配）。
    const extra = buildCursorExtra({
      access_token: accessToken,
      refresh_token: refreshToken,
      machine_id: machineId,
      mac_machine_id: macMachineId,
      config_version: configVersion,
      timezone,
      proxy,
    })

    createMutation.mutate(
      {
        account_id: accountId,
        provider: 'cursor',
        group: group !== '' ? group : undefined,
        max_concurrency: parsedConcurrency,
        priority: tierToPriority(priorityTier),
        egress,
        extra,
      },
      {
        onSuccess: onClose,
        onError: (err) => {
          const status = getErrorStatus(err)
          if (status === 409) setError(t('accounts.error.duplicate'))
          else setError(extractErrorMessage(err))
        },
      },
    )
  }

  return (
    <Modal open={open} onClose={onClose} title={t('accounts.cursor.title')}>
      <div className="mt-4 space-y-4">
        <p className="flex items-start gap-1.5 rounded-2xl border border-dashed border-primary/30 bg-primary/5 p-3 text-xs text-muted-foreground">
          <Info className="mt-0.5 h-3.5 w-3.5 shrink-0 text-primary" />
          {t('accounts.cursor.intro')}
        </p>

        <div className="space-y-1.5">
          <label htmlFor="cursor-account-id" className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.id')}
          </label>
          <input
            id="cursor-account-id"
            value={accountId}
            onChange={(event) => setAccountId(event.target.value)}
            placeholder={t('accounts.cursor.idPlaceholder')}
            spellCheck={false}
            autoComplete="off"
            autoFocus
            className={`${inputClass} font-mono`}
          />
          <p className="text-xs text-muted-foreground">{t('accounts.field.idRule')}</p>
        </div>

        <div className="space-y-1.5">
          <label htmlFor="cursor-group" className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.group')}
          </label>
          <Select
            id="cursor-group"
            value={group}
            onChange={(event) => setGroup(event.target.value)}
            className="w-full"
          >
            <option value="">{t('groups.ungrouped')}</option>
            {(groupsQuery.data ?? []).map((g) => (
              <option key={g.name} value={g.name}>
                {g.name}
              </option>
            ))}
          </Select>
        </div>

        <div className="space-y-1.5">
          <label
            htmlFor="cursor-concurrency"
            className="text-xs font-medium text-muted-foreground"
          >
            {t('accounts.field.concurrency')}
          </label>
          <input
            id="cursor-concurrency"
            type="number"
            min={1}
            step={1}
            value={concurrency}
            onChange={(event) => setConcurrency(event.target.value)}
            className={inputClass}
          />
        </div>

        {/* 调度优先级：只作建号时的默认种子（顶层 priority 字段），真正的按组排序在编辑弹窗里设。 */}
        <div className="space-y-1.5">
          <span className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.priority')}
          </span>
          <div className="flex flex-wrap items-center gap-2">
            <Segment
              options={[
                { value: 'high' as const, label: t('accounts.priorityTier.high') },
                { value: 'low' as const, label: t('accounts.priorityTier.low') },
              ]}
              value={priorityTier}
              onChange={setPriorityTier}
            />
          </div>
          <p className="text-xs text-muted-foreground">{t('accounts.field.priorityHint')}</p>
        </div>

        <div className="space-y-1.5">
          <label htmlFor="cursor-egress" className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.egress')}
          </label>
          <Select
            id="cursor-egress"
            value={egress}
            onChange={(event) => setEgress(event.target.value)}
            className="w-full"
          >
            <option value="auto">{t('accounts.egress.auto')}</option>
            <option value="direct">{t('accounts.egress.direct')}</option>
            {gateways.map((url, i) => (
              <option key={i} value={String(i)}>
                {gatewayLabel(url, i)}
              </option>
            ))}
          </Select>
          <p className="text-xs text-muted-foreground">{t('accounts.cursor.egressHint')}</p>
        </div>

        {/* 凭据：access_token 必填；refresh_token 选填但强烈建议（留空 = 无法自动续期）。 */}
        <div className="space-y-1.5">
          <label
            htmlFor="cursor-access-token"
            className="text-xs font-medium text-muted-foreground"
          >
            {t('accounts.cursor.accessToken')}
          </label>
          <input
            id="cursor-access-token"
            type="password"
            value={accessToken}
            onChange={(event) => setAccessToken(event.target.value)}
            placeholder={t('accounts.cursor.accessTokenPlaceholder')}
            spellCheck={false}
            autoComplete="off"
            className={`${inputClass} font-mono`}
          />
        </div>

        <div className="space-y-1.5">
          <label
            htmlFor="cursor-refresh-token"
            className="text-xs font-medium text-muted-foreground"
          >
            {t('accounts.cursor.refreshToken')}
          </label>
          <input
            id="cursor-refresh-token"
            type="password"
            value={refreshToken}
            onChange={(event) => setRefreshToken(event.target.value)}
            placeholder={t('accounts.cursor.refreshTokenPlaceholder')}
            spellCheck={false}
            autoComplete="off"
            className={`${inputClass} font-mono`}
          />
          <p className="text-xs text-muted-foreground">{t('accounts.cursor.refreshTokenHint')}</p>
        </div>

        {/* 高级项：设备指纹 / config_version / 时区 / 代理，全部可留空（后端派生或自动分配）。 */}
        <div className="space-y-1.5">
          <button
            type="button"
            onClick={() => setAdvancedOpen((prev) => !prev)}
            className="inline-flex items-center gap-1 rounded text-xs font-medium text-muted-foreground transition-colors hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
          >
            <ChevronRight
              className={cn('h-3.5 w-3.5 transition-transform', advancedOpen && 'rotate-90')}
            />
            {t('accounts.cursor.advancedToggle')}
          </button>
          {advancedOpen && (
            <div className="space-y-4 rounded-2xl border border-dashed border-primary/30 bg-primary/5 p-3">
              <div className="space-y-1.5">
                <label
                  htmlFor="cursor-machine-id"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t('accounts.cursor.machineId')}
                </label>
                <input
                  id="cursor-machine-id"
                  value={machineId}
                  onChange={(event) => setMachineId(event.target.value)}
                  placeholder={t('accounts.cursor.machineIdPlaceholder')}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} font-mono`}
                />
                <p className="text-xs text-muted-foreground">{t('accounts.cursor.machineIdHint')}</p>
              </div>

              <div className="space-y-1.5">
                <label
                  htmlFor="cursor-mac-machine-id"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t('accounts.cursor.macMachineId')}
                </label>
                <input
                  id="cursor-mac-machine-id"
                  value={macMachineId}
                  onChange={(event) => setMacMachineId(event.target.value)}
                  placeholder={t('accounts.cursor.machineIdPlaceholder')}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} font-mono`}
                />
                <p className="text-xs text-muted-foreground">
                  {t('accounts.cursor.macMachineIdHint')}
                </p>
              </div>

              <div className="space-y-1.5">
                <label
                  htmlFor="cursor-config-version"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t('accounts.cursor.configVersion')}
                </label>
                <input
                  id="cursor-config-version"
                  value={configVersion}
                  onChange={(event) => setConfigVersion(event.target.value)}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} font-mono`}
                />
                <p className="text-xs text-muted-foreground">
                  {t('accounts.cursor.configVersionHint')}
                </p>
              </div>

              <div className="space-y-1.5">
                <label
                  htmlFor="cursor-timezone"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t('accounts.cursor.timezone')}
                </label>
                <input
                  id="cursor-timezone"
                  value={timezone}
                  onChange={(event) => setTimezone(event.target.value)}
                  placeholder={t('accounts.cursor.timezonePlaceholder')}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} font-mono`}
                />
                <p className="text-xs text-muted-foreground">{t('accounts.cursor.timezoneHint')}</p>
              </div>

              <div className="space-y-1.5">
                <label
                  htmlFor="cursor-proxy"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t('accounts.field.proxy')}
                </label>
                <input
                  id="cursor-proxy"
                  value={proxy}
                  onChange={(event) => setProxy(event.target.value)}
                  placeholder={t('accounts.field.proxyPlaceholder')}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} font-mono`}
                />
                <p className="text-xs text-muted-foreground">{t('accounts.cursor.proxyHint')}</p>
              </div>
            </div>
          )}
        </div>

        {error !== null && <p className="text-sm text-destructive">{error}</p>}

        <div className="flex justify-end gap-2 pt-1">
          <Button variant="ghost" onClick={onClose}>
            {t('common.cancel')}
          </Button>
          <Button onClick={handleSubmit} disabled={createMutation.isPending}>
            {createMutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
            {createMutation.isPending ? t('accounts.cursor.creating') : t('accounts.cursor.submit')}
          </Button>
        </div>
      </div>
    </Modal>
  )
}
