import { DEFAULT_GROUP_COLOR } from '../types'

interface GroupChipProps {
  /** 分组名；空串 = 未分组（显示 —）。 */
  name: string
  /** 分组颜色（hex）；缺省/空串时用默认色。 */
  color?: string
}

/** 色点 + 组名的小 chip，用于账号表 / Key 表的「分组」列。 */
export function GroupChip({ name, color }: GroupChipProps) {
  if (!name) return <span className="text-muted-foreground">—</span>

  const resolved = color || DEFAULT_GROUP_COLOR
  return (
    <span
      className="inline-flex max-w-36 items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium"
      style={{
        backgroundColor: `color-mix(in srgb, ${resolved} 14%, transparent)`,
        color: resolved,
      }}
      title={name}
    >
      <span className="h-1.5 w-1.5 shrink-0 rounded-full" style={{ backgroundColor: resolved }} />
      <span className="truncate">{name}</span>
    </span>
  )
}
