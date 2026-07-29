import { useEffect, useState } from 'react'
import { CheckCircle2, ChevronRight, Copy, ExternalLink, Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { useSettings } from '@/features/settings/hooks'
import { extractErrorMessage, getErrorStatus } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { useCreateAccount, useOAuthComplete, useOAuthStart } from '../hooks'
import { parseConcurrency } from '../lib'
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

interface OAuthAccountDialogProps {
  open: boolean
  onClose: () => void
}

/**
 * claude-dario OAuth 上号对话框(两步)。
 * 步骤1:填账号信息 → 生成 authorize URL(后端纯本地,不发网络)。
 * 步骤2:操作员浏览器登录同意拿 code → 换码(后端扇给目标组 worker,走该组 egress=该号
 * 将来 refresh/chat 同一出口 IP)→ 落库。consent 浏览器在哪登都行(code 数秒失效)。
 *
 * 另有一条**折叠的旁路**:手里已经有 `.credentials.json` 时直接粘贴 → `POST /accounts`
 * 建号,跳过整个授权流程。这条路原本在已删除的「添加账号」弹窗里。
 */
export function OAuthAccountDialog({ open, onClose }: OAuthAccountDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const settingsQuery = useSettings()
  const gateways = settingsQuery.data?.egress_pool ?? []
  const startMutation = useOAuthStart()
  const completeMutation = useOAuthComplete()
  const createMutation = useCreateAccount()

  const [accountId, setAccountId] = useState('')
  const [group, setGroup] = useState('')
  const [concurrency, setConcurrency] = useState('2')
  const [egress, setEgress] = useState('direct')
  // 步骤2 状态:start 成功后填入 authorize URL + state。
  const [authorizeUrl, setAuthorizeUrl] = useState('')
  const [state, setState] = useState('')
  const [code, setCode] = useState('')
  const [copied, setCopied] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // 旁路:手里已有 .credentials.json 时直接建号,不走授权。
  const [pasteOpen, setPasteOpen] = useState(false)
  const [credentialsJson, setCredentialsJson] = useState('')

  useEffect(() => {
    if (open) {
      setAccountId('')
      setGroup('')
      setConcurrency('2')
      setEgress('direct')
      setAuthorizeUrl('')
      setState('')
      setCode('')
      setCopied(false)
      setError(null)
      setPasteOpen(false)
      setCredentialsJson('')
    }
  }, [open])

  /** 表单头部三项(账号 ID / 并发)的公共校验;返回 null = 已置错误消息。 */
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

  const handlePasteCreate = () => {
    if (createMutation.isPending) return
    const validated = validateForm()
    if (validated === null) return
    if (credentialsJson.trim() === '') {
      setError(t('accounts.error.credentialsJsonRequired'))
      return
    }
    setError(null)
    createMutation.mutate(
      {
        account_id: accountId,
        provider: 'claude-dario',
        group: group !== '' ? group : undefined,
        max_concurrency: validated.concurrency,
        egress,
        credentials_json: credentialsJson.trim(),
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

  const step = authorizeUrl === '' ? 'form' : 'authorize'

  const handleGenerate = () => {
    if (startMutation.isPending) return
    const validated = validateForm()
    if (validated === null) return
    setError(null)
    startMutation.mutate(
      {
        account_id: accountId,
        group: group !== '' ? group : undefined,
        egress,
        max_concurrency: validated.concurrency,
      },
      {
        onSuccess: (res) => {
          setAuthorizeUrl(res.authorize_url)
          setState(res.state)
        },
        onError: (err) => {
          const status = getErrorStatus(err)
          if (status === 409) setError(t('accounts.error.duplicate'))
          else setError(extractErrorMessage(err))
        },
      },
    )
  }

  const handleCopy = () => {
    void navigator.clipboard?.writeText(authorizeUrl).then(() => {
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1500)
    })
  }

  const handleComplete = () => {
    if (completeMutation.isPending) return
    if (code.trim() === '') {
      setError(t('accounts.oauth.error.codeRequired'))
      return
    }
    setError(null)
    completeMutation.mutate(
      { state, code: code.trim() },
      {
        onSuccess: onClose,
        onError: (err) => setError(extractErrorMessage(err)),
      },
    )
  }

  return (
    <Modal open={open} onClose={onClose} title={t('accounts.oauth.title')}>
      <div className="mt-4 space-y-4">
        <p className="flex items-start gap-1.5 rounded-2xl border border-dashed border-primary/30 bg-primary/5 p-3 text-xs text-muted-foreground">
          <Info className="mt-0.5 h-3.5 w-3.5 shrink-0 text-primary" />
          {t('accounts.oauth.intro')}
        </p>

        {step === 'form' && (
          <>
            <div className="space-y-1.5">
              <label htmlFor="oauth-account-id" className="text-xs font-medium text-muted-foreground">
                {t('accounts.field.id')}
              </label>
              <input
                id="oauth-account-id"
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

            <div className="space-y-1.5">
              <label htmlFor="oauth-group" className="text-xs font-medium text-muted-foreground">
                {t('accounts.field.group')}
              </label>
              <Select
                id="oauth-group"
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
              <label htmlFor="oauth-concurrency" className="text-xs font-medium text-muted-foreground">
                {t('accounts.field.concurrency')}
              </label>
              <input
                id="oauth-concurrency"
                type="number"
                min={1}
                step={1}
                value={concurrency}
                onChange={(event) => setConcurrency(event.target.value)}
                className={inputClass}
              />
            </div>

            <div className="space-y-1.5">
              <label htmlFor="oauth-egress" className="text-xs font-medium text-muted-foreground">
                {t('accounts.field.egress')}
              </label>
              <Select
                id="oauth-egress"
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
              <p className="text-xs text-muted-foreground">{t('accounts.oauth.egressHint')}</p>
            </div>

            {/* 旁路:已有 .credentials.json 就不必再走一遍授权。默认折叠。 */}
            <div className="space-y-1.5">
              <button
                type="button"
                onClick={() => setPasteOpen((prev) => !prev)}
                className="inline-flex items-center gap-1 rounded text-xs font-medium text-muted-foreground transition-colors hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
              >
                <ChevronRight
                  className={cn('h-3.5 w-3.5 transition-transform', pasteOpen && 'rotate-90')}
                />
                {t('accounts.oauth.pasteToggle')}
              </button>
              {pasteOpen && (
                <div className="space-y-1.5 rounded-2xl border border-dashed border-primary/30 bg-primary/5 p-3">
                  <label
                    htmlFor="oauth-cred-json"
                    className="text-xs font-medium text-muted-foreground"
                  >
                    {t('accounts.field.credentialsJson')}
                  </label>
                  <textarea
                    id="oauth-cred-json"
                    value={credentialsJson}
                    onChange={(event) => setCredentialsJson(event.target.value)}
                    placeholder={t('accounts.field.credentialsJsonPlaceholder')}
                    rows={4}
                    spellCheck={false}
                    autoComplete="off"
                    className={`${inputClass} resize-none font-mono text-xs leading-5`}
                  />
                  <p className="text-xs text-muted-foreground">
                    {t('accounts.field.credentialsJsonHint')}
                  </p>
                  <div className="flex justify-end pt-0.5">
                    <Button
                      variant="outline"
                      onClick={handlePasteCreate}
                      disabled={createMutation.isPending}
                    >
                      {createMutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
                      {createMutation.isPending
                        ? t('accounts.oauth.pasteCreating')
                        : t('accounts.oauth.pasteSubmit')}
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
              <Button onClick={handleGenerate} disabled={startMutation.isPending}>
                {startMutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
                {startMutation.isPending
                  ? t('accounts.oauth.generating')
                  : t('accounts.oauth.generate')}
              </Button>
            </div>
          </>
        )}

        {step === 'authorize' && (
          <>
            <p className="text-xs text-muted-foreground">{t('accounts.oauth.authorizeHint')}</p>

            <div className="flex gap-2">
              <a
                href={authorizeUrl}
                target="_blank"
                rel="noreferrer"
                className="inline-flex flex-1 items-center justify-center gap-1.5 rounded-2xl bg-primary px-4 py-2.5 text-sm font-medium text-primary-foreground transition-colors hover:opacity-90"
              >
                <ExternalLink className="h-4 w-4" />
                {t('accounts.oauth.openAuthorize')}
              </a>
              <Button variant="outline" onClick={handleCopy}>
                {copied ? <CheckCircle2 className="h-4 w-4 text-success" /> : <Copy className="h-4 w-4" />}
                {copied ? t('accounts.oauth.copied') : t('accounts.oauth.copyLink')}
              </Button>
            </div>

            <div className="space-y-1.5">
              <label htmlFor="oauth-code" className="text-xs font-medium text-muted-foreground">
                {t('accounts.oauth.codeLabel')}
              </label>
              <textarea
                id="oauth-code"
                value={code}
                onChange={(event) => setCode(event.target.value)}
                placeholder={t('accounts.oauth.codePlaceholder')}
                rows={2}
                spellCheck={false}
                autoComplete="off"
                className={`${inputClass} resize-none font-mono text-xs leading-5`}
              />
            </div>

            {error !== null && <p className="text-sm text-destructive">{error}</p>}

            <div className="flex justify-between gap-2 pt-1">
              <Button
                variant="ghost"
                onClick={() => {
                  setAuthorizeUrl('')
                  setState('')
                  setCode('')
                  setError(null)
                }}
              >
                {t('accounts.oauth.restart')}
              </Button>
              <Button onClick={handleComplete} disabled={completeMutation.isPending}>
                {completeMutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
                {completeMutation.isPending
                  ? t('accounts.oauth.completing')
                  : t('accounts.oauth.complete')}
              </Button>
            </div>
          </>
        )}
      </div>
    </Modal>
  )
}
