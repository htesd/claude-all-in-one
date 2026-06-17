import { useEffect, useState, type FormEvent } from 'react'
import { Info, Loader2, Wand2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { useSettings } from '@/features/settings/hooks'
import { extractErrorMessage, getErrorStatus } from '@/lib/api'
import { useI18n } from '@/lib/i18n'

import { useCreateAccount } from '../hooks'
import { parseConcurrency } from '../lib'
import { ACCOUNT_ID_PATTERN, type CreateAccountPayload } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm outline-none transition-colors placeholder:text-muted-foreground'

/** 识别出的账号类型(决定字段与封号风险提示)。 */
type DetectedType = 'social' | 'builderid' | 'idc' | 'idc-like' | null

/** 清洗成合法 account_id（对齐后端 ACCOUNT_ID_PATTERN：字母数字 + . _ ~ -，1–64）。 */
function sanitizeAccountId(raw: string): string {
  const s = raw
    .replace(/[^A-Za-z0-9._~-]/g, '-')
    .slice(0, 64)
    .replace(/-+$/, '')
  return s === '' ? 'kiro-account' : s
}

/** 从对象里按多个候选键(snake/camel)取第一个非空字符串。 */
function pickStr(o: Record<string, unknown>, ...keys: string[]): string {
  for (const k of keys) {
    const v = o[k]
    if (typeof v === 'string' && v.trim() !== '') return v.trim()
  }
  return ''
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

interface CreateAccountDialogProps {
  open: boolean
  onClose: () => void
}

/** 添加账号对话框：account_id + 分组 + refresh_token（必填）+ 并发数（默认 1）。 */
export function CreateAccountDialog({ open, onClose }: CreateAccountDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const settingsQuery = useSettings()
  const gateways = settingsQuery.data?.egress_pool ?? []
  const mutation = useCreateAccount()

  const [provider, setProvider] = useState<'kiro' | 'claude-dario'>('kiro')
  const [accountId, setAccountId] = useState('')
  const [group, setGroup] = useState('')
  const [token, setToken] = useState('')
  const [clientId, setClientId] = useState('')
  const [clientSecret, setClientSecret] = useState('')
  const [region, setRegion] = useState('')
  const [machineId, setMachineId] = useState('')
  const [concurrency, setConcurrency] = useState('2')
  const [egress, setEgress] = useState('auto')
  const [paste, setPaste] = useState('')
  const [detected, setDetected] = useState<DetectedType>(null)
  /** claude-dario 专用：粘贴 .credentials.json 全文。 */
  const [credentialsJson, setCredentialsJson] = useState('')
  const [error, setError] = useState<string | null>(null)

  const isDario = provider === 'claude-dario'

  // 每次打开都从干净的表单态开始
  useEffect(() => {
    if (open) {
      setProvider('kiro')
      setAccountId('')
      setGroup('')
      setToken('')
      setClientId('')
      setClientSecret('')
      setRegion('')
      setMachineId('')
      setConcurrency('2')
      setEgress('auto')
      setPaste('')
      setDetected(null)
      setCredentialsJson('')
      setError(null)
    }
  }, [open])

  // 智能填充:粘贴一个账号的 JSON(camelCase/snake_case、KiroManager 嵌套都认),
  // 自动抽取字段并识别类型。非 JSON 文本视为纯 refresh_token(social)。
  const autofill = (text: string) => {
    setPaste(text)
    const trimmed = text.trim()
    if (trimmed === '') {
      setDetected(null)
      return
    }
    let obj: Record<string, unknown> | null = null
    try {
      const parsed: unknown = JSON.parse(trimmed)
      if (Array.isArray(parsed)) obj = (parsed[0] ?? null) as Record<string, unknown> | null
      else if (parsed && typeof parsed === 'object') obj = parsed as Record<string, unknown>
    } catch {
      obj = null
    }
    if (!obj) {
      // 不是 JSON → 当作纯 refresh_token(social)。
      setToken(trimmed)
      setClientId('')
      setClientSecret('')
      setDetected('social')
      return
    }
    // KiroManager 嵌套:凭据在 credentials 子对象;否则字段就在顶层。
    const credsVal = obj.credentials
    const creds =
      credsVal && typeof credsVal === 'object' ? (credsVal as Record<string, unknown>) : obj

    const rt = pickStr(creds, 'refresh_token', 'refreshToken')
    const cid = pickStr(creds, 'client_id', 'clientId')
    const cs = pickStr(creds, 'client_secret', 'clientSecret')
    const reg = pickStr(creds, 'region', 'auth_region', 'authRegion')
    const mid = pickStr(obj, 'machine_id', 'machineId') || pickStr(creds, 'machine_id', 'machineId')
    const email = pickStr(obj, 'email')
    const idSrc = email || pickStr(obj, 'user_id', 'userId')
    const provider =
      pickStr(obj, 'provider', 'idp') || pickStr(creds, 'provider', 'auth_method', 'authMethod')

    if (rt) setToken(rt)
    setClientId(cid)
    setClientSecret(cs)
    setRegion(reg)
    setMachineId(mid)
    if (accountId.trim() === '' && idSrc) setAccountId(sanitizeAccountId(idSrc))

    // 类型识别:有 client 凭据 → 看 provider 区分 BuilderId / IdC;没有 → social。
    if (cid && cs) {
      const p = provider.toLowerCase()
      if (p.includes('builder')) setDetected('builderid')
      else if (p.includes('idc') || p.includes('identity') || p.includes('iam') || p.includes('enterprise'))
        setDetected('idc')
      else setDetected('idc-like')
    } else {
      setDetected('social')
    }
  }

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (mutation.isPending) return

    if (!ACCOUNT_ID_PATTERN.test(accountId)) {
      setError(t('accounts.error.invalidId'))
      return
    }

    const parsedConcurrency = parseConcurrency(concurrency)
    if (parsedConcurrency === null) {
      setError(t('accounts.error.invalidConcurrency'))
      return
    }

    if (isDario && credentialsJson.trim() === '') {
      setError(t('accounts.error.credentialsJsonRequired'))
      return
    }

    setError(null)

    let payload: CreateAccountPayload

    if (isDario) {
      // claude-dario 路径：credentials_json 是主要凭据来源（可为空，后端允许）
      payload = {
        account_id: accountId,
        provider: 'claude-dario',
        max_concurrency: parsedConcurrency,
        egress,
        credentials_json: credentialsJson.trim() || undefined,
      }
    } else {
      // kiro 路径（原有逻辑）
      const trimmedToken = token.trim()
      if (trimmedToken === '') {
        setError(t('accounts.error.tokenRequired'))
        return
      }
      // Client ID / Secret 必须成对(IdC/BuilderId 两者都需要;social 两者都空)。
      const cid = clientId.trim()
      const cs = clientSecret.trim()
      if ((cid === '') !== (cs === '')) {
        setError(t('accounts.error.clientPair'))
        return
      }
      const extra: Record<string, unknown> = { refresh_token: trimmedToken }
      if (cid !== '') extra.client_id = cid
      if (cs !== '') extra.client_secret = cs
      const trimmedRegion = region.trim()
      if (trimmedRegion !== '') extra.region = trimmedRegion
      const trimmedMachineId = machineId.trim()
      if (trimmedMachineId !== '') extra.machine_id = trimmedMachineId

      payload = {
        account_id: accountId,
        max_concurrency: parsedConcurrency,
        extra,
        egress,
      }
    }

    if (group !== '') payload.group = group

    mutation.mutate(payload, {
      onSuccess: onClose,
      onError: (err) => {
        // 409 = ID 重复，400 = 格式非法（含凭证解析失败），其余透出服务端 message
        const status = getErrorStatus(err)
        if (status === 409) setError(t('accounts.error.duplicate'))
        else setError(extractErrorMessage(err))
      },
    })
  }

  return (
    <Modal open={open} onClose={onClose} title={t('accounts.create.title')}>
      <form onSubmit={handleSubmit} className="mt-4 space-y-4">

        {/* 账号类型（provider）选择器 */}
        <div className="space-y-1.5">
          <label htmlFor="account-provider" className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.provider')}
          </label>
          <Select
            id="account-provider"
            value={provider}
            onChange={(event) => setProvider(event.target.value as 'kiro' | 'claude-dario')}
            className="w-full"
          >
            <option value="kiro">{t('accounts.provider.kiro')}</option>
            <option value="claude-dario">{t('accounts.provider.claudeDario')}</option>
          </Select>
        </div>

        {/* ── kiro 路径：智能填充 + refresh_token + Client ID/Secret 等 ── */}
        {!isDario && (
          <>
            {/* 智能填充:粘贴账号 JSON → 自动识别类型并填充下方字段 */}
            <div className="space-y-1.5 rounded-2xl border border-dashed border-primary/30 bg-primary/5 p-3">
              <label
                htmlFor="account-paste"
                className="flex items-center gap-1.5 text-xs font-medium text-primary"
              >
                <Wand2 className="h-3.5 w-3.5" />
                {t('accounts.field.smartFill')}
              </label>
              <textarea
                id="account-paste"
                value={paste}
                onChange={(event) => autofill(event.target.value)}
                placeholder={t('accounts.field.smartFillPlaceholder')}
                rows={2}
                spellCheck={false}
                autoComplete="off"
                className={`${inputClass} resize-none font-mono text-xs leading-5`}
              />
              {detected !== null && (
                <p className="flex items-center gap-1.5 text-xs">
                  <span className="text-muted-foreground">{t('accounts.type.detected')}</span>
                  {detected === 'builderid' && (
                    <span className="font-medium text-success">{t('accounts.type.builderid')}</span>
                  )}
                  {detected === 'social' && (
                    <span className="font-medium text-success">{t('accounts.type.social')}</span>
                  )}
                  {detected === 'idc' && (
                    <span className="font-medium text-warning">{t('accounts.type.idc')}</span>
                  )}
                  {detected === 'idc-like' && (
                    <span className="font-medium text-foreground">{t('accounts.type.idcLike')}</span>
                  )}
                </p>
              )}
              <p className="text-xs text-muted-foreground">{t('accounts.field.smartFillHint')}</p>
            </div>
          </>
        )}

        {/* account_id */}
        <div className="space-y-1.5">
          <label htmlFor="account-id" className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.id')}
          </label>
          <input
            id="account-id"
            value={accountId}
            onChange={(event) => setAccountId(event.target.value)}
            placeholder={t('accounts.field.idPlaceholder')}
            spellCheck={false}
            autoComplete="off"
            autoFocus
            className={`${inputClass} font-mono`}
          />
          <p className="text-xs text-muted-foreground">{t('accounts.field.idRule')}</p>
        </div>

        {/* ── claude-dario 路径：粘贴 .credentials.json ── */}
        {isDario && (
          <div className="space-y-1.5 rounded-2xl border border-dashed border-primary/30 bg-primary/5 p-3">
            <label
              htmlFor="account-cred-json"
              className="text-xs font-medium text-muted-foreground"
            >
              {t('accounts.field.credentialsJson')}
            </label>
            <textarea
              id="account-cred-json"
              value={credentialsJson}
              onChange={(event) => setCredentialsJson(event.target.value)}
              placeholder={t('accounts.field.credentialsJsonPlaceholder')}
              rows={4}
              spellCheck={false}
              autoComplete="off"
              className={`${inputClass} resize-none font-mono text-xs leading-5`}
            />
            <p className="text-xs text-muted-foreground">{t('accounts.field.credentialsJsonHint')}</p>
          </div>
        )}

        {/* ── kiro 路径：refresh_token + Client ID/Secret ── */}
        {!isDario && (
          <>
            {/* refresh_token（必填） */}
            <div className="space-y-1.5">
              <label htmlFor="account-token" className="text-xs font-medium text-muted-foreground">
                {t('accounts.field.refreshToken')}
              </label>
              <textarea
                id="account-token"
                value={token}
                onChange={(event) => setToken(event.target.value)}
                placeholder={t('accounts.field.refreshTokenPlaceholder')}
                rows={3}
                spellCheck={false}
                autoComplete="off"
                className={`${inputClass} resize-none font-mono text-xs leading-5`}
              />
            </div>

            {/* 凭据（IdC / BuilderId 需填 Client ID + Secret；social 两者留空） */}
            <div className="space-y-3 rounded-2xl border border-dashed border-black/10 p-3 dark:border-white/10">
              <p className="text-xs text-muted-foreground">{t('accounts.field.credHint')}</p>
              <div className="space-y-1.5">
                <label htmlFor="account-client-id" className="text-xs font-medium text-muted-foreground">
                  {t('accounts.field.clientId')}
                </label>
                <input
                  id="account-client-id"
                  value={clientId}
                  onChange={(event) => setClientId(event.target.value)}
                  placeholder={t('accounts.field.clientIdPlaceholder')}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} font-mono text-xs`}
                />
              </div>
              <div className="space-y-1.5">
                <label
                  htmlFor="account-client-secret"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t('accounts.field.clientSecret')}
                </label>
                <textarea
                  id="account-client-secret"
                  value={clientSecret}
                  onChange={(event) => setClientSecret(event.target.value)}
                  placeholder={t('accounts.field.clientSecretPlaceholder')}
                  rows={2}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} resize-none font-mono text-xs leading-5`}
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-1.5">
                  <label htmlFor="account-region" className="text-xs font-medium text-muted-foreground">
                    {t('accounts.field.region')}
                  </label>
                  <input
                    id="account-region"
                    value={region}
                    onChange={(event) => setRegion(event.target.value)}
                    placeholder={t('accounts.field.regionPlaceholder')}
                    spellCheck={false}
                    autoComplete="off"
                    className={`${inputClass} font-mono text-xs`}
                  />
                </div>
                <div className="space-y-1.5">
                  <label
                    htmlFor="account-machine-id"
                    className="text-xs font-medium text-muted-foreground"
                  >
                    {t('accounts.field.machineId')}
                  </label>
                  <input
                    id="account-machine-id"
                    value={machineId}
                    onChange={(event) => setMachineId(event.target.value)}
                    placeholder={t('accounts.field.machineIdPlaceholder')}
                    spellCheck={false}
                    autoComplete="off"
                    className={`${inputClass} font-mono text-xs`}
                  />
                </div>
              </div>
              <p className="text-xs text-muted-foreground">{t('accounts.field.machineIdHint')}</p>
            </div>
          </>
        )}

        {/* 分组 */}
        <div className="space-y-1.5">
          <label htmlFor="account-group" className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.group')}
          </label>
          <Select
            id="account-group"
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

        {/* 并发上限 */}
        <div className="space-y-1.5">
          <label
            htmlFor="account-concurrency"
            className="text-xs font-medium text-muted-foreground"
          >
            {t('accounts.field.concurrency')}
          </label>
          <input
            id="account-concurrency"
            type="number"
            min={1}
            step={1}
            value={concurrency}
            onChange={(event) => setConcurrency(event.target.value)}
            className={inputClass}
          />
        </div>

        {/* 出口网关（上号时选：直连 / 自动均衡 / 指定网关） */}
        <div className="space-y-1.5">
          <label htmlFor="account-egress" className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.egress')}
          </label>
          <Select
            id="account-egress"
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

        {error !== null && <p className="text-sm text-destructive">{error}</p>}

        {/* worker 周期同步提示 */}
        <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Info className="h-3.5 w-3.5 shrink-0" />
          {t('accounts.syncHint')}
        </p>

        <div className="flex justify-end gap-2 pt-1">
          <Button variant="ghost" onClick={onClose}>
            {t('common.cancel')}
          </Button>
          <Button type="submit" disabled={mutation.isPending}>
            {mutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
            {mutation.isPending ? t('accounts.create.creating') : t('accounts.create.submit')}
          </Button>
        </div>
      </form>
    </Modal>
  )
}
