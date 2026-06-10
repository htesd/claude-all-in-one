import { AlertTriangle } from 'lucide-react'

import { extractErrorMessage } from '@/lib/api'
import { useI18n, type I18nKey } from '@/lib/i18n'

interface ErrorNoteProps {
  error: unknown
  /** 前缀文案，默认"加载失败"；mutation 场景可传 'common.actionFailed'。 */
  labelKey?: I18nKey
}

export function ErrorNote({ error, labelKey = 'common.loadFailed' }: ErrorNoteProps) {
  const { t } = useI18n()
  return (
    <div className="glass-card flex items-center gap-2 rounded-xl px-4 py-3 text-sm text-destructive">
      <AlertTriangle className="h-4 w-4 shrink-0" />
      <span>
        {t(labelKey)}: {extractErrorMessage(error)}
      </span>
    </div>
  )
}
