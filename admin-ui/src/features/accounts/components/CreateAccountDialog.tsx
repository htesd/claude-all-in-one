import { useEffect, useState, type FormEvent } from 'react'
import { Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { extractErrorMessage, getErrorStatus } from '@/lib/api'
import { useI18n } from '@/lib/i18n'

import { useCreateAccount } from '../hooks'
import { parseConcurrency } from '../lib'
import { ACCOUNT_ID_PATTERN, type CreateAccountPayload } from '../types'

const inputClass =
  'w-full rounded-xl border bg-input px-3 py-2 text-sm text-foreground transition-colors placeholder:text-muted-foreground focus:outline-none'

interface CreateAccountDialogProps {
  open: boolean
  onClose: () => void
}

/** 添加账号对话框：account_id + 分组 + refresh_token（必填）+ 并发数（默认 1）。 */
export function CreateAccountDialog({ open, onClose }: CreateAccountDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const mutation = useCreateAccount()

  const [accountId, setAccountId] = useState('')
  const [group, setGroup] = useState('')
  const [token, setToken] = useState('')
  const [concurrency, setConcurrency] = useState('2')
  const [proxy, setProxy] = useState('')
  const [error, setError] = useState<string | null>(null)

  // 每次打开都从干净的表单态开始
  useEffect(() => {
    if (open) {
      setAccountId('')
      setGroup('')
      setToken('')
      setConcurrency('2')
      setProxy('')
      setError(null)
    }
  }, [open])

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (mutation.isPending) return

    if (!ACCOUNT_ID_PATTERN.test(accountId)) {
      setError(t('accounts.error.invalidId'))
      return
    }
    // refresh_token 必填；粘贴常带换行，trim 后入库
    const trimmedToken = token.trim()
    if (trimmedToken === '') {
      setError(t('accounts.error.tokenRequired'))
      return
    }
    const parsedConcurrency = parseConcurrency(concurrency)
    if (parsedConcurrency === null) {
      setError(t('accounts.error.invalidConcurrency'))
      return
    }
    setError(null)

    const extra: Record<string, unknown> = { refresh_token: trimmedToken }
    const trimmedProxy = proxy.trim()
    if (trimmedProxy !== '') extra.proxy = trimmedProxy

    const payload: CreateAccountPayload = {
      account_id: accountId,
      max_concurrency: parsedConcurrency,
      extra,
    }
    if (group !== '') payload.group = group

    mutation.mutate(payload, {
      onSuccess: onClose,
      onError: (err) => {
        // 409 = ID 重复，400 = 格式非法，其余透出服务端 message
        const status = getErrorStatus(err)
        if (status === 409) setError(t('accounts.error.duplicate'))
        else if (status === 400) setError(t('accounts.error.invalidId'))
        else setError(extractErrorMessage(err))
      },
    })
  }

  return (
    <Modal open={open} onClose={onClose} title={t('accounts.create.title')}>
      <form onSubmit={handleSubmit} className="mt-4 space-y-4">
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

        {/* 出口代理（可选） */}
        <div className="space-y-1.5">
          <label htmlFor="account-proxy" className="text-xs font-medium text-muted-foreground">
            {t('accounts.field.proxy')}
          </label>
          <input
            id="account-proxy"
            type="text"
            value={proxy}
            onChange={(event) => setProxy(event.target.value)}
            placeholder={t('accounts.field.proxyPlaceholder')}
            spellCheck={false}
            autoComplete="off"
            className={inputClass}
          />
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
