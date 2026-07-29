import { useEffect, useRef, useState, type ChangeEvent, type FormEvent } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import {
  AlertCircle,
  CheckCircle2,
  Clock,
  Info,
  Loader2,
  Trash2,
  Upload,
  XCircle,
} from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { useSettings } from '@/features/settings/hooks'
import { extractErrorMessage, getErrorStatus } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { queryKeys } from '@/lib/query-keys'

import { deleteAccount, fetchAccountQuotaNow } from '../api'
import { useImportAccounts, useImportApiKeys } from '../hooks'
import { formatCredits } from '../lib'
import type { AccountQuota, ImportAccountsResult } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm outline-none transition-colors placeholder:text-muted-foreground'

/** 导入后 worker 未同步(404)时的重试节奏:admin 已主动捅 /sync,通常首试即中;
 * 这里兜底覆盖 worker 繁忙/降级场景,上限 ≈ 一个 30s 周期。 */
const SYNC_RETRY_DELAY_MS = 4_000
const SYNC_MAX_ATTEMPTS = 8

/** 单账号验活状态机(对齐 kiro.rs batch-import-dialog 的逐条状态)。 */
type VerifyState =
  | { phase: 'pending' }
  | { phase: 'waiting' } // worker 尚未同步到该账号,重试中
  | { phase: 'verifying' }
  | { phase: 'ok'; quota: AccountQuota }
  | { phase: 'noQuota' } // 可刷新但上游无配额数据
  | { phase: 'failed'; error: string }

const TERMINAL_PHASES = new Set(['ok', 'noQuota', 'failed'])

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

/** 把网关代理 URL 显示成「序号. host:port」(密码段已被后端掩码,host 仍可识别)。 */
function gatewayLabel(url: string, i: number): string {
  let host = url
  try {
    host = new URL(url).host || url
  } catch {
    host = url
  }
  return `${i + 1}. ${host}`
}

interface ImportAccountsDialogProps {
  open: boolean
  onClose: () => void
}

/** 导入账号导出 JSON(完整导入 + 智能合并),随后**逐账号自动验活**
 * (只读:刷新 token + 查配额,绝不发 chat)。 */
export function ImportAccountsDialog({ open, onClose }: ImportAccountsDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const settingsQuery = useSettings()
  const gateways = settingsQuery.data?.egress_pool ?? []
  const mutation = useImportAccounts()
  const apiKeysMutation = useImportApiKeys()
  const queryClient = useQueryClient()
  const fileInputRef = useRef<HTMLInputElement>(null)
  // 运行序号(generation token):开/关对话框都自增,使仍在 await 中的旧验活/删除
  // 循环失效——不能用布尔 cancelled(重开会复位,4s 睡眠里的旧循环会"复活"写进新会话)。
  const runSeqRef = useRef(0)

  // 导入模式:'json' = KiroManager 导出 JSON;'apikeys' = 粘贴官方 API Key(ksk_)列表。
  const [mode, setMode] = useState<'json' | 'apikeys'>('json')
  const [group, setGroup] = useState('')
  const [json, setJson] = useState('')
  const [keys, setKeys] = useState('')
  const [egress, setEgress] = useState('auto')
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<ImportAccountsResult | null>(null)

  const [verifyStates, setVerifyStates] = useState<Record<string, VerifyState>>({})
  const [verifying, setVerifying] = useState(false)
  const [removing, setRemoving] = useState(false)
  const [removedIds, setRemovedIds] = useState<Set<string>>(new Set())

  useEffect(() => {
    runSeqRef.current += 1
    if (open) {
      setMode('json')
      setGroup('')
      setJson('')
      setKeys('')
      setEgress('auto')
      setError(null)
      setResult(null)
      setVerifyStates({})
      setVerifying(false)
      setRemoving(false)
      setRemovedIds(new Set())
    }
  }, [open])

  const handleFile = (event: ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0]
    if (!file) return
    const reader = new FileReader()
    reader.onload = () => setJson(typeof reader.result === 'string' ? reader.result : '')
    reader.readAsText(file)
  }

  const invalidateAccountDomains = () => {
    void queryClient.invalidateQueries({ queryKey: queryKeys.accounts.root })
    void queryClient.invalidateQueries({ queryKey: queryKeys.groups.root })
  }

  /** 逐账号串行验活(skipped 不验);404 = worker 未同步 → 限次重试。
   * 验活是只读探测:关闭对话框即取消(运行序号失配),剩余账号交给后台轮询。 */
  const runVerification = async (items: ImportAccountsResult['items']) => {
    const run = runSeqRef.current
    const stale = () => runSeqRef.current !== run
    const targets = items.filter((i) => i.action !== 'skipped')
    if (targets.length === 0) return
    setVerifying(true)
    setVerifyStates(
      Object.fromEntries(
        targets.map((i): [string, VerifyState] => [i.account_id, { phase: 'pending' }]),
      ),
    )
    try {
      for (const item of targets) {
        if (stale()) return
        const id = item.account_id
        setVerifyStates((prev) => ({ ...prev, [id]: { phase: 'verifying' } }))
        let state: VerifyState | null = null
        for (let attempt = 1; attempt <= SYNC_MAX_ATTEMPTS; attempt++) {
          try {
            const res = await fetchAccountQuotaNow(id)
            state =
              res.verified && res.quota
                ? { phase: 'ok', quota: res.quota }
                : { phase: 'noQuota' }
            break
          } catch (err) {
            if (getErrorStatus(err) === 404 && attempt < SYNC_MAX_ATTEMPTS) {
              // worker 尚未同步到该账号:等待后重试。
              if (stale()) return
              setVerifyStates((prev) => ({ ...prev, [id]: { phase: 'waiting' } }))
              await sleep(SYNC_RETRY_DELAY_MS)
              continue
            }
            state =
              getErrorStatus(err) === 404
                ? { phase: 'failed', error: t('accounts.import.waitingSyncTimeout') }
                : { phase: 'failed', error: extractErrorMessage(err) }
            break
          }
        }
        if (stale()) return
        setVerifyStates((prev) => ({ ...prev, [id]: state ?? { phase: 'noQuota' } }))
      }
    } finally {
      // 取消/完成都收尾:验活已更新配额缓存、可能标了死号禁用 → 刷新表格。
      // setVerifying 只在本运行仍有效时写,避免覆盖重开后新会话的状态。
      if (!stale()) setVerifying(false)
      invalidateAccountDomains()
    }
  }

  const submitting = mutation.isPending || apiKeysMutation.isPending

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (submitting) return
    const onSuccess = (data: ImportAccountsResult) => {
      setResult(data)
      void runVerification(data.items)
    }
    const onError = (err: unknown) => setError(extractErrorMessage(err))

    if (mode === 'apikeys') {
      if (keys.trim() === '') {
        setError(t('accounts.import.keysEmpty'))
        return
      }
      setError(null)
      apiKeysMutation.mutate(
        { keys, group_name: group || undefined, egress },
        { onSuccess, onError },
      )
      return
    }

    if (json.trim() === '') {
      setError(t('accounts.import.empty'))
      return
    }
    setError(null)
    mutation.mutate(
      { json, group_name: group || undefined, egress },
      { onSuccess, onError },
    )
  }

  const conflictCount =
    result?.items.filter((i) => i.machine_id_conflict).length ?? 0

  const targets = (result?.items ?? []).filter((i) => i.action !== 'skipped')
  const terminalCount = targets.filter((i) =>
    TERMINAL_PHASES.has(verifyStates[i.account_id]?.phase ?? ''),
  ).length
  const okCount = targets.filter((i) => verifyStates[i.account_id]?.phase === 'ok').length
  const noQuotaCount = targets.filter(
    (i) => verifyStates[i.account_id]?.phase === 'noQuota',
  ).length
  const failCount = targets.filter(
    (i) => verifyStates[i.account_id]?.phase === 'failed',
  ).length
  // 仅"本次新建且验活失败"的账号可一键删除(merged 是已有账号,绝不连带删)。
  const failedCreatedIds = targets
    .filter(
      (i) =>
        i.action === 'created' &&
        verifyStates[i.account_id]?.phase === 'failed' &&
        !removedIds.has(i.account_id),
    )
    .map((i) => i.account_id)

  const handleRemoveFailed = async () => {
    if (removing || failedCreatedIds.length === 0) return
    const run = runSeqRef.current
    const stale = () => runSeqRef.current !== run
    setRemoving(true)
    setError(null)
    const removed = new Set(removedIds)
    try {
      for (const id of failedCreatedIds) {
        try {
          await deleteAccount(id)
          removed.add(id)
          if (stale()) return
          setRemovedIds(new Set(removed))
        } catch (err) {
          if (stale()) return
          setError(`${id}: ${extractErrorMessage(err)}`)
        }
      }
    } finally {
      if (!stale()) setRemoving(false)
      invalidateAccountDomains()
    }
  }

  // 只有**写操作**(导入提交/删除)进行中不允许关闭;验活是只读探测,关闭即取消
  // (审查 Architect#4/Minimalist#4:worker 离线时串行重试可达几十分钟,必须给取消路径)。
  const busy = submitting || removing
  const safeClose = () => {
    if (busy) return
    onClose()
  }

  const verifyBadge = (state: VerifyState | undefined, removed: boolean) => {
    if (removed) {
      return (
        <span className="inline-flex items-center gap-1 text-xs text-muted-foreground line-through">
          {t('accounts.import.removedTag')}
        </span>
      )
    }
    if (!state) return null
    switch (state.phase) {
      case 'verifying':
        return (
          <span className="inline-flex items-center gap-1 text-xs text-sky-600 dark:text-sky-400">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('accounts.import.verifying')}
          </span>
        )
      case 'waiting':
        return (
          <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('accounts.import.verifyWaitingSync')}
          </span>
        )
      case 'ok': {
        const q = state.quota
        return (
          <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
            <CheckCircle2 className="h-3.5 w-3.5" />
            {t('accounts.import.verifyOk')}
            <span className="tabular-nums" title={q.label ?? undefined}>
              {formatCredits(q.remaining)}
              <span className="text-muted-foreground"> / {formatCredits(q.limit)}</span>
            </span>
          </span>
        )
      }
      case 'noQuota':
        return (
          <span className="inline-flex items-center gap-1 text-xs text-warning">
            <AlertCircle className="h-3.5 w-3.5" />
            {t('accounts.import.verifyNoQuota')}
          </span>
        )
      case 'failed':
        return (
          <span
            className="inline-flex max-w-64 items-center gap-1 text-xs text-destructive"
            title={state.error}
          >
            <XCircle className="h-3.5 w-3.5 shrink-0" />
            <span className="truncate">
              {t('accounts.import.verifyFail')}: {state.error}
            </span>
          </span>
        )
      case 'pending':
        return (
          <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
            <Clock className="h-3.5 w-3.5" />
            {t('accounts.import.verifyPending')}
          </span>
        )
      default:
        return null
    }
  }

  return (
    <Modal open={open} onClose={safeClose} title={t('accounts.import.title')} className="max-w-xl">
      {result !== null ? (
        // 导入结果 + 逐账号验活。
        <div className="mt-4 space-y-4">
          <div className="flex flex-wrap gap-3 text-sm">
            <span className="rounded-2xl bg-emerald-500/10 px-3 py-1.5 font-medium text-emerald-600 dark:text-emerald-400">
              {t('accounts.import.created')} {result.created}
            </span>
            <span className="rounded-2xl bg-sky-500/10 px-3 py-1.5 font-medium text-sky-600 dark:text-sky-400">
              {t('accounts.import.merged')} {result.merged}
            </span>
            <span className="rounded-2xl bg-muted px-3 py-1.5 font-medium text-muted-foreground">
              {t('accounts.import.skipped')} {result.skipped}
            </span>
          </div>
          {conflictCount > 0 && (
            <p className="flex items-start gap-1.5 text-xs text-warning">
              <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>
                {conflictCount} {t('accounts.import.machineIdConflict')}
              </span>
            </p>
          )}

          {/* 验活进度 */}
          {targets.length > 0 && (
            <div className="space-y-1.5">
              <div className="flex items-center justify-between text-xs font-medium text-muted-foreground">
                <span>{t('accounts.import.verifyTitle')}</span>
                <span className="tabular-nums">
                  {terminalCount} / {targets.length}
                </span>
              </div>
              <div className="h-2 w-full overflow-hidden rounded-full bg-black/[0.08] dark:bg-white/10">
                <div
                  className="h-2 rounded-full bg-acid transition-all"
                  style={{ width: `${(terminalCount / targets.length) * 100}%` }}
                />
              </div>
              <div className="flex gap-3 text-xs">
                <span className="text-emerald-600 dark:text-emerald-400">
                  ✓ {t('accounts.import.verifySummaryOk')}: {okCount}
                </span>
                <span className="text-warning">
                  ⚠ {t('accounts.import.verifySummaryNoQuota')}: {noQuotaCount}
                </span>
                <span className="text-destructive">
                  ✗ {t('accounts.import.verifySummaryFail')}: {failCount}
                </span>
              </div>
            </div>
          )}

          <ul className="max-h-56 space-y-1.5 overflow-y-auto text-xs">
            {result.items.map((item) => (
              <li
                key={item.account_id}
                className="flex items-center justify-between gap-2"
              >
                <span className="flex min-w-0 items-center gap-2">
                  <code className="truncate font-mono text-muted-foreground">
                    {item.account_id}
                  </code>
                  <span className="shrink-0 text-muted-foreground">
                    {item.action === 'created'
                      ? t('accounts.import.created')
                      : item.action === 'merged'
                        ? t('accounts.import.merged')
                        : t('accounts.import.skipped')}
                  </span>
                </span>
                <span className="shrink-0">
                  {item.action === 'skipped'
                    ? null
                    : verifyBadge(verifyStates[item.account_id], removedIds.has(item.account_id))}
                </span>
              </li>
            ))}
          </ul>

          {/* 验活失败的新建账号:一键删除(merged 不连带) */}
          {!verifying && failedCreatedIds.length > 0 && (
            <Button
              variant="ghost"
              className="text-destructive"
              disabled={removing}
              onClick={() => void handleRemoveFailed()}
            >
              {removing ? (
                <Loader2 className="h-4 w-4 animate-spin" />
              ) : (
                <Trash2 className="h-4 w-4" />
              )}
              {t('accounts.import.removeFailed')} ({failedCreatedIds.length})
            </Button>
          )}

          {error !== null && <p className="text-sm text-destructive">{error}</p>}

          <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Info className="h-3.5 w-3.5 shrink-0" />
            {t('accounts.import.verifyHint')}
          </p>
          <div className="flex justify-end pt-1">
            <Button onClick={safeClose} disabled={busy}>
              {verifying ? t('accounts.import.cancelVerify') : t('common.cancel')}
            </Button>
          </div>
        </div>
      ) : (
        <form onSubmit={handleSubmit} className="mt-4 space-y-4">
          {/* 导入方式:两栏等宽卡片。以前是个灰底小 tab,用户找不到 API Key 这条路,
              以为压根没实现 —— 入口显眼程度本身就是功能的一部分。 */}
          <div className="grid grid-cols-2 gap-2">
            {(
              [
                { value: 'json', label: 'modeJson', desc: 'modeJsonDesc' },
                { value: 'apikeys', label: 'modeApiKeys', desc: 'modeApiKeysDesc' },
              ] as const
            ).map((option) => (
              <button
                key={option.value}
                type="button"
                onClick={() => setMode(option.value)}
                aria-pressed={mode === option.value}
                className={`rounded-2xl border p-3 text-left transition-colors ${
                  mode === option.value
                    ? 'border-primary bg-primary/5 ring-1 ring-primary/40'
                    : 'hover:bg-muted/60'
                }`}
              >
                <span className="block text-sm font-medium">
                  {t(`accounts.import.${option.label}`)}
                </span>
                <span className="mt-0.5 block text-xs text-muted-foreground">
                  {t(`accounts.import.${option.desc}`)}
                </span>
              </button>
            ))}
          </div>

          {/* 目标分组 */}
          <div className="space-y-1.5">
            <label htmlFor="import-group" className="text-xs font-medium text-muted-foreground">
              {t('accounts.import.group')}
            </label>
            <Select
              id="import-group"
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

          {/* 出口网关（上号时选：直连 / 自动均衡 / 指定网关） */}
          <div className="space-y-1.5">
            <label htmlFor="import-egress" className="text-xs font-medium text-muted-foreground">
              {t('accounts.field.egress')}
            </label>
            <Select
              id="import-egress"
              value={egress}
              onChange={(event) => setEgress(event.target.value)}
              className="w-full"
            >
              <option value="direct">{t('accounts.egress.direct')}</option>
              <option value="auto">{t('accounts.egress.auto')}</option>
              {gateways.map((url, i) => (
                <option key={i} value={String(i)}>
                  {gatewayLabel(url, i)}
                </option>
              ))}
            </Select>
          </div>

          {mode === 'json' ? (
            /* JSON 粘贴 + 文件选择 */
            <div className="space-y-1.5">
              <div className="flex items-center justify-between">
                <label htmlFor="import-json" className="text-xs font-medium text-muted-foreground">
                  {t('accounts.import.jsonLabel')}
                </label>
                <button
                  type="button"
                  onClick={() => fileInputRef.current?.click()}
                  className="inline-flex items-center gap-1 text-xs text-primary hover:underline"
                >
                  <Upload className="h-3 w-3" />
                  {t('accounts.import.chooseFile')}
                </button>
                <input
                  ref={fileInputRef}
                  type="file"
                  accept=".json,application/json"
                  className="hidden"
                  onChange={handleFile}
                />
              </div>
              <textarea
                id="import-json"
                value={json}
                onChange={(event) => setJson(event.target.value)}
                placeholder={t('accounts.import.jsonPlaceholder')}
                rows={8}
                spellCheck={false}
                autoComplete="off"
                className={`${inputClass} resize-none font-mono text-xs leading-5`}
              />
            </div>
          ) : (
            /* API Key 粘贴(每行一个 ksk_...) */
            <div className="space-y-1.5">
              <label htmlFor="import-keys" className="text-xs font-medium text-muted-foreground">
                {t('accounts.import.keysLabel')}
              </label>
              <textarea
                id="import-keys"
                value={keys}
                onChange={(event) => setKeys(event.target.value)}
                placeholder={t('accounts.import.keysPlaceholder')}
                rows={8}
                spellCheck={false}
                autoComplete="off"
                className={`${inputClass} resize-none font-mono text-xs leading-5`}
              />
            </div>
          )}

          {error !== null && <p className="text-sm text-destructive">{error}</p>}

          <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
            <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            {mode === 'apikeys' ? t('accounts.import.keysHint') : t('accounts.import.hint')}
          </p>

          <div className="flex justify-end gap-2 pt-1">
            <Button variant="ghost" onClick={safeClose}>
              {t('common.cancel')}
            </Button>
            <Button type="submit" disabled={submitting}>
              {submitting && <Loader2 className="h-4 w-4 animate-spin" />}
              {submitting ? t('accounts.import.importing') : t('accounts.import.submit')}
            </Button>
          </div>
        </form>
      )}
    </Modal>
  )
}
