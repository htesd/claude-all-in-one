import { useEffect, useRef, useState } from 'react'
import { CheckCircle2, ChevronRight, Copy, ExternalLink, Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Segment } from '@/components/ui/segment'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { useSettings } from '@/features/settings/hooks'
import { extractErrorMessage, getErrorStatus } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { useCreateAccount, useCursorLoginPoll, useCursorLoginStart } from '../hooks'
import {
  buildCursorExtra,
  decideCursorLoginPoll,
  parseConcurrency,
  tierToPriority,
  type PriorityTier,
} from '../lib'
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
 * Cursor 账号建号对话框（两步，照 OAuthAccountDialog 范式）。
 *
 * 默认推荐入口 = 官方登录（PKCE + 轮询）：
 * 步骤1:填账号信息 → `POST /accounts/cursor/login/start` 拿 login_url（纯本地，不发上游，
 *       重名 409 / 分组不存在在此前置拦截，不让操作员登完浏览器才撞错）。
 * 步骤2:展示 login_url（复制 / 浏览器打开）→ 按 poll_interval_sec 轮询
 *       `POST /accounts/cursor/login/poll`，done 即已落库（关弹窗刷新列表）；
 *       502/传输错误继续轮询，4xx 终态报错，超过 expires_in_sec 报超时。
 *       没有取消端点：用户取消 = 前端停止轮询，会话 15 分钟后自行过期。
 *
 * 另有一条**折叠的旁路**：手里已有凭据（真机 state.vscdb 抄出的 access/refresh token）
 * 时直接粘贴 → `POST /accounts`（provider: 'cursor'），跳过整个登录流程。
 */
export function CursorAccountDialog({ open, onClose }: CursorAccountDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const settingsQuery = useSettings()
  const gateways = settingsQuery.data?.egress_pool ?? []
  const createMutation = useCreateAccount()
  const startMutation = useCursorLoginStart()
  const pollMutation = useCursorLoginPoll()

  const [accountId, setAccountId] = useState('')
  const [group, setGroup] = useState('')
  const [concurrency, setConcurrency] = useState('1')
  const [priorityTier, setPriorityTier] = useState<PriorityTier>('low')
  const [egress, setEgress] = useState('auto')
  // 步骤2 状态：start 成功后填入登录链接与轮询参数。
  const [loginUrl, setLoginUrl] = useState('')
  const [flowId, setFlowId] = useState('')
  const [pollIntervalMs, setPollIntervalMs] = useState(2000)
  const [expiresAt, setExpiresAt] = useState(0)
  const [copied, setCopied] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // 旁路：手里已有凭据时直接建号，不走官方登录。默认折叠。
  const [manualOpen, setManualOpen] = useState(false)
  const [accessToken, setAccessToken] = useState('')
  const [refreshToken, setRefreshToken] = useState('')
  const [advancedOpen, setAdvancedOpen] = useState(false)
  const [machineId, setMachineId] = useState('')
  const [macMachineId, setMacMachineId] = useState('')
  const [configVersion, setConfigVersion] = useState('')
  const [timezone, setTimezone] = useState('')
  const [proxy, setProxy] = useState('')

  // onClose / poll 的 mutateAsync 走 ref：轮询 effect 只在会话参数变化时重建，
  // 不因父组件 15s 运行态刷新带来的新函数身份而重启轮询循环。
  const onCloseRef = useRef(onClose)
  useEffect(() => {
    onCloseRef.current = onClose
  }, [onClose])
  const pollAsyncRef = useRef(pollMutation.mutateAsync)
  useEffect(() => {
    pollAsyncRef.current = pollMutation.mutateAsync
  }, [pollMutation.mutateAsync])

  // 关窗后置保护:start/copy 的异步回调在弹窗已关闭时不再 setState(复审低危)。
  const openRef = useRef(open)
  useEffect(() => {
    openRef.current = open
  }, [open])
  // 复制反馈的 1.5s 定时器:关窗/卸载时清掉,不留孤儿定时器。
  const copyTimerRef = useRef(0)
  useEffect(() => {
    if (!open) window.clearTimeout(copyTimerRef.current)
  }, [open])
  useEffect(() => () => window.clearTimeout(copyTimerRef.current), [])

  useEffect(() => {
    if (open) {
      setAccountId('')
      setGroup('')
      setConcurrency('1')
      setPriorityTier('low')
      setEgress('auto')
      setLoginUrl('')
      setFlowId('')
      setPollIntervalMs(2000)
      setExpiresAt(0)
      setCopied(false)
      setError(null)
      setManualOpen(false)
      setAccessToken('')
      setRefreshToken('')
      setAdvancedOpen(false)
      setMachineId('')
      setMacMachineId('')
      setConfigVersion('')
      setTimezone('')
      setProxy('')
    }
  }, [open])

  const step = flowId === '' ? 'form' : 'waiting'

  // 步骤2 的轮询循环：每次 tick 结束按 pollIntervalMs 排下一趟；
  // continue 之外的决策（success/timeout/fail）都不再排程，循环自然停止。
  useEffect(() => {
    if (!open || flowId === '') return
    let stopped = false
    let timer = 0
    const schedule = () => {
      timer = window.setTimeout(tick, pollIntervalMs)
    }
    const tick = () => {
      // 发请求前先判超时:截止后不再多发请求(复审中危)。
      if (Date.now() >= expiresAt) {
        setError(t('accounts.cursor.login.timeout'))
        return
      }
      pollAsyncRef.current(flowId).then(
        (result) => {
          if (stopped) return
          // 超时以响应返回时刻为准:请求可能跨越截止线,不能用发请求前捕获的值
          //(done 仍优先于 timeout,见 decideCursorLoginPoll 契约与用例)。
          const action = decideCursorLoginPoll(
            result.done ? { kind: 'done' } : { kind: 'pending' },
            Date.now() >= expiresAt,
          )
          if (action === 'success') {
            // 账号已落库（hook 里已失效账号域查询），关弹窗即刷新列表。
            onCloseRef.current()
            return
          }
          if (action === 'timeout') {
            setError(t('accounts.cursor.login.timeout'))
            return
          }
          schedule()
        },
        (err: unknown) => {
          if (stopped) return
          const action = decideCursorLoginPoll(
            { kind: 'error', status: getErrorStatus(err) },
            Date.now() >= expiresAt,
          )
          if (action === 'timeout') {
            setError(t('accounts.cursor.login.timeout'))
            return
          }
          if (action === 'fail') {
            setError(extractErrorMessage(err))
            return
          }
          // 502 / 传输层抖动：会话还在，按原节奏继续问。
          schedule()
        },
      )
    }
    schedule()
    return () => {
      stopped = true
      window.clearTimeout(timer)
    }
  }, [open, flowId, expiresAt, pollIntervalMs, t])

  /** 表单头部三项（账号 ID / 并发）的公共校验；返回 null = 已置错误消息。 */
  const validateForm = (): { concurrency: number } | null => {
    if (!ACCOUNT_ID_PATTERN.test(accountId)) {
      setError(t('accounts.error.invalidId'))
      return null
    }
    const parsedConcurrency = parseConcurrency(concurrency)
    if (parsedConcurrency === null) {
      setError(t('accounts.error.invalidConcurrency'))
      return null
    }
    return { concurrency: parsedConcurrency }
  }

  const handleStartLogin = () => {
    if (startMutation.isPending) return
    const validated = validateForm()
    if (validated === null) return
    setError(null)
    startMutation.mutate(
      {
        account_id: accountId,
        group: group !== '' ? group : undefined,
        max_concurrency: validated.concurrency,
        priority: tierToPriority(priorityTier),
        egress,
      },
      {
        onSuccess: (res) => {
          if (!openRef.current) return
          setLoginUrl(res.login_url)
          setFlowId(res.flow_id)
          setPollIntervalMs(Math.max(1, res.poll_interval_sec) * 1000)
          setExpiresAt(Date.now() + Math.max(1, res.expires_in_sec) * 1000)
        },
        onError: (err) => {
          if (!openRef.current) return
          const status = getErrorStatus(err)
          if (status === 409) setError(t('accounts.error.duplicate'))
          else setError(extractErrorMessage(err))
        },
      },
    )
  }

  const handleCopy = () => {
    void navigator.clipboard?.writeText(loginUrl).then(() => {
      if (!openRef.current) return
      setCopied(true)
      window.clearTimeout(copyTimerRef.current)
      copyTimerRef.current = window.setTimeout(() => {
        if (openRef.current) setCopied(false)
      }, 1500)
    })
  }

  /** 放弃本次登录会话（前端停止轮询即可，后端会话 15 分钟自行过期），回到表单。 */
  const handleRestart = () => {
    setLoginUrl('')
    setFlowId('')
    setCopied(false)
    setError(null)
  }

  const handleManualSubmit = () => {
    if (createMutation.isPending) return
    const validated = validateForm()
    if (validated === null) return
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
        max_concurrency: validated.concurrency,
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

        {step === 'form' && (
          <>
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

            {/* 旁路：已有凭据（state.vscdb 抄出的 token）就不必再走一遍官方登录。默认折叠。 */}
            <div className="space-y-1.5">
              <button
                type="button"
                onClick={() => setManualOpen((prev) => !prev)}
                className="inline-flex items-center gap-1 rounded text-xs font-medium text-muted-foreground transition-colors hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
              >
                <ChevronRight
                  className={cn('h-3.5 w-3.5 transition-transform', manualOpen && 'rotate-90')}
                />
                {t('accounts.cursor.manualToggle')}
              </button>
              {manualOpen && (
                <div className="space-y-4 rounded-2xl border border-dashed border-primary/30 bg-primary/5 p-3">
                  <p className="text-xs text-muted-foreground">{t('accounts.cursor.manualHint')}</p>

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
                    <p className="text-xs text-muted-foreground">
                      {t('accounts.cursor.refreshTokenHint')}
                    </p>
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
                          <p className="text-xs text-muted-foreground">
                            {t('accounts.cursor.machineIdHint')}
                          </p>
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
                          <p className="text-xs text-muted-foreground">
                            {t('accounts.cursor.timezoneHint')}
                          </p>
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
                          <p className="text-xs text-muted-foreground">
                            {t('accounts.cursor.proxyHint')}
                          </p>
                        </div>
                      </div>
                    )}
                  </div>

                  <div className="flex justify-end pt-0.5">
                    <Button
                      variant="outline"
                      onClick={handleManualSubmit}
                      disabled={createMutation.isPending}
                    >
                      {createMutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
                      {createMutation.isPending
                        ? t('accounts.cursor.creating')
                        : t('accounts.cursor.submit')}
                    </Button>
                  </div>
                </div>
              )}
            </div>

            {error !== null && <p className="text-sm text-destructive">{error}</p>}

            <div className="flex justify-end gap-2 pt-1">
              <Button variant="ghost" onClick={onClose}>
                {t('common.cancel')}
              </Button>
              <Button onClick={handleStartLogin} disabled={startMutation.isPending}>
                {startMutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
                {startMutation.isPending
                  ? t('accounts.cursor.login.starting')
                  : t('accounts.cursor.login.submit')}
              </Button>
            </div>
          </>
        )}

        {step === 'waiting' && (
          <>
            <p className="text-xs text-muted-foreground">
              {t('accounts.cursor.login.authorizeHint')}
            </p>

            <div className="flex gap-2">
              <a
                href={loginUrl}
                target="_blank"
                rel="noreferrer"
                className="inline-flex flex-1 items-center justify-center gap-1.5 rounded-2xl bg-primary px-4 py-2.5 text-sm font-medium text-primary-foreground transition-colors hover:opacity-90"
              >
                <ExternalLink className="h-4 w-4" />
                {t('accounts.cursor.login.openPage')}
              </a>
              <Button variant="outline" onClick={handleCopy}>
                {copied ? <CheckCircle2 className="h-4 w-4 text-success" /> : <Copy className="h-4 w-4" />}
                {copied ? t('accounts.cursor.login.copied') : t('accounts.cursor.login.copyLink')}
              </Button>
            </div>

            {error === null && (
              <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                {t('accounts.cursor.login.waiting')}
              </p>
            )}

            {error !== null && <p className="text-sm text-destructive">{error}</p>}

            <div className="flex justify-between gap-2 pt-1">
              <Button variant="ghost" onClick={handleRestart}>
                {t('accounts.cursor.login.restart')}
              </Button>
              <Button variant="outline" onClick={onClose}>
                {t('common.cancel')}
              </Button>
            </div>
          </>
        )}
      </div>
    </Modal>
  )
}
