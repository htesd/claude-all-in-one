import { useEffect, useRef, useState, type ChangeEvent, type FormEvent } from 'react'
import { Info, Loader2, Upload } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { extractErrorMessage } from '@/lib/api'
import { useI18n } from '@/lib/i18n'

import { useImportAccounts } from '../hooks'
import type { ImportAccountsResult } from '../types'

const inputClass =
  'w-full rounded-xl border bg-input px-3 py-2 text-sm text-foreground transition-colors placeholder:text-muted-foreground focus:outline-none'

interface ImportAccountsDialogProps {
  open: boolean
  onClose: () => void
}

/** 导入 KiroManager 导出 JSON(完整导入 + 智能合并)。 */
export function ImportAccountsDialog({ open, onClose }: ImportAccountsDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const mutation = useImportAccounts()
  const fileInputRef = useRef<HTMLInputElement>(null)

  const [group, setGroup] = useState('')
  const [json, setJson] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<ImportAccountsResult | null>(null)

  useEffect(() => {
    if (open) {
      setGroup('')
      setJson('')
      setError(null)
      setResult(null)
    }
  }, [open])

  const handleFile = (event: ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0]
    if (!file) return
    const reader = new FileReader()
    reader.onload = () => setJson(typeof reader.result === 'string' ? reader.result : '')
    reader.readAsText(file)
  }

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (mutation.isPending) return
    if (json.trim() === '') {
      setError(t('accounts.import.empty'))
      return
    }
    setError(null)
    mutation.mutate(
      { json, group_name: group || undefined },
      {
        onSuccess: (data) => setResult(data),
        onError: (err) => setError(extractErrorMessage(err)),
      },
    )
  }

  const conflictCount =
    result?.items.filter((i) => i.machine_id_conflict).length ?? 0

  return (
    <Modal open={open} onClose={onClose} title={t('accounts.import.title')}>
      {result !== null ? (
        // 导入结果汇总。
        <div className="mt-4 space-y-4">
          <div className="flex flex-wrap gap-3 text-sm">
            <span className="rounded-lg bg-emerald-500/10 px-3 py-1.5 font-medium text-emerald-600 dark:text-emerald-400">
              {t('accounts.import.created')} {result.created}
            </span>
            <span className="rounded-lg bg-sky-500/10 px-3 py-1.5 font-medium text-sky-600 dark:text-sky-400">
              {t('accounts.import.merged')} {result.merged}
            </span>
            <span className="rounded-lg bg-muted px-3 py-1.5 font-medium text-muted-foreground">
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
          <ul className="max-h-48 space-y-1 overflow-y-auto text-xs">
            {result.items.map((item) => (
              <li key={item.account_id} className="flex items-center justify-between gap-2">
                <code className="truncate font-mono text-muted-foreground">{item.account_id}</code>
                <span className="shrink-0">
                  {item.action === 'created'
                    ? t('accounts.import.created')
                    : item.action === 'merged'
                      ? t('accounts.import.merged')
                      : t('accounts.import.skipped')}
                </span>
              </li>
            ))}
          </ul>
          <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Info className="h-3.5 w-3.5 shrink-0" />
            {t('accounts.syncHint')}
          </p>
          <div className="flex justify-end pt-1">
            <Button onClick={onClose}>{t('common.cancel')}</Button>
          </div>
        </div>
      ) : (
        <form onSubmit={handleSubmit} className="mt-4 space-y-4">
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

          {/* JSON 粘贴 + 文件选择 */}
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

          {error !== null && <p className="text-sm text-destructive">{error}</p>}

          <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
            <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            {t('accounts.import.hint')}
          </p>

          <div className="flex justify-end gap-2 pt-1">
            <Button variant="ghost" onClick={onClose}>
              {t('common.cancel')}
            </Button>
            <Button type="submit" disabled={mutation.isPending}>
              {mutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
              {mutation.isPending ? t('accounts.import.importing') : t('accounts.import.submit')}
            </Button>
          </div>
        </form>
      )}
    </Modal>
  )
}
