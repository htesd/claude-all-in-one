import { AlertTriangle } from 'lucide-react'

import { extractErrorMessage } from '@/lib/api'
import { useI18n } from '@/lib/i18n'

export function ErrorNote({ error }: { error: unknown }) {
  const { t } = useI18n()
  return (
    <div className="glass-card flex items-center gap-2 rounded-xl px-4 py-3 text-sm text-destructive">
      <AlertTriangle className="h-4 w-4 shrink-0" />
      <span>
        {t('common.loadFailed')}: {extractErrorMessage(error)}
      </span>
    </div>
  )
}
