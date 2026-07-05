import { Segment } from '@/components/ui/segment'
import { useI18n } from '@/lib/i18n'

export type StatusFilter = 'all' | 'ok' | 'abnormal' | 'disabled'
export type TierFilter = 'all' | 'PRO' | 'POWER' | 'FREE' | 'OTHER'

interface AccountsFilterBarProps {
  /** 当前状态筛选值。 */
  statusFilter: StatusFilter
  onStatusChange: (v: StatusFilter) => void

  /** 当前提供方筛选值（'all' = 全部）。 */
  providerFilter: string
  onProviderChange: (v: string) => void
  /** 数据中实际存在的 provider 列表（含计数），用于渲染选项。 */
  providers: Array<{ provider: string; count: number }>

  /** 当前订阅档筛选值（'all' = 全部）。 */
  tierFilter: TierFilter
  onTierChange: (v: TierFilter) => void
  /** 数据中存在的订阅档选项；为空或 provider 不是 kiro/all 时隐藏档位筛选。 */
  tiers: TierFilter[]
  /** 当前筛选的 provider 是否属于需要展示档位的范围。 */
  showTierFilter: boolean
}

/** 账号列表筛选条：状态 / 提供方 / 订阅档（均受控，父组件持状态）。 */
export function AccountsFilterBar({
  statusFilter,
  onStatusChange,
  providerFilter,
  onProviderChange,
  providers,
  tierFilter,
  onTierChange,
  tiers,
  showTierFilter,
}: AccountsFilterBarProps) {
  const { t } = useI18n()

  const statusOptions = [
    { value: 'all' as const, label: t('filter.statusAll') },
    { value: 'ok' as const, label: t('filter.statusOk') },
    { value: 'abnormal' as const, label: t('filter.statusAbnormal') },
    { value: 'disabled' as const, label: t('filter.statusDisabled') },
  ]

  const providerOptions = [
    { value: 'all', label: t('filter.providerAll') },
    ...providers.map((p) => ({ value: p.provider, label: `${p.provider} (${p.count})` })),
  ]

  const tierOptions: Array<{ value: TierFilter; label: string }> = [
    { value: 'all', label: t('filter.tierAll') },
    ...tiers.map((tier) => ({ value: tier, label: tier })),
  ]

  return (
    <div className="flex flex-wrap items-center gap-x-6 gap-y-3">
      {/* 状态 */}
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium text-muted-foreground">{t('filter.status')}</span>
        <Segment options={statusOptions} value={statusFilter} onChange={onStatusChange} />
      </div>

      {/* 提供方 */}
      {providers.length >= 1 && (
        <div className="flex items-center gap-2">
          <span className="text-xs font-medium text-muted-foreground">{t('filter.provider')}</span>
          <Segment options={providerOptions} value={providerFilter} onChange={onProviderChange} />
        </div>
      )}

      {/* 订阅档：仅在 kiro/all 且有可选档位时展示 */}
      {showTierFilter && tiers.length > 0 && (
        <div className="flex items-center gap-2">
          <span className="text-xs font-medium text-muted-foreground">{t('filter.tier')}</span>
          <Segment options={tierOptions} value={tierFilter} onChange={onTierChange} />
        </div>
      )}
    </div>
  )
}
