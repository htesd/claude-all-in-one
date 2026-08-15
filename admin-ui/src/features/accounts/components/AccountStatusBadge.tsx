import { Badge } from '@/components/ui/badge'
import { useI18n } from '@/lib/i18n'

import type { AccountDisplayStatus } from '../types'

/** 运行状态徽章：灰（停用/离线）、绿（正常）、橙（冷却 + 剩余秒数）、红（不可用）。 */
export function AccountStatusBadge({ status }: { status: AccountDisplayStatus }) {
  const { t } = useI18n()

  switch (status.kind) {
    case 'disabled':
      return <Badge variant="muted">{t('accounts.status.disabled')}</Badge>
    case 'offline':
      return <Badge variant="muted">{t('accounts.status.offline')}</Badge>
    case 'ok':
      return <Badge variant="success">{t('accounts.status.normal')}</Badge>
    case 'probation':
      return <Badge variant="warning">{t('accounts.status.probation')}</Badge>
    case 'suspended':
      return (
        <Badge variant="warning">
          {t('accounts.status.suspended')} {status.secs}s
        </Badge>
      )
    case 'retired':
      return <Badge variant="destructive">{t('accounts.status.retired')}</Badge>
    case 'rate_limited':
      return (
        <Badge variant="warning">
          {t('accounts.status.rateLimited')} {status.secs}s
        </Badge>
      )
    case 'empty_response':
      return (
        <Badge variant="warning">
          {t('accounts.status.emptyResponse')} {status.secs}s
        </Badge>
      )
    case 'quota_exhausted':
      return <Badge variant="destructive">{t('accounts.status.quotaExhausted')}</Badge>
    case 'invalid_refresh_token':
      return <Badge variant="destructive">{t('accounts.status.invalidToken')}</Badge>
    case 'too_many_failures':
      return <Badge variant="destructive">{t('accounts.status.tooManyFailures')}</Badge>
  }
}
