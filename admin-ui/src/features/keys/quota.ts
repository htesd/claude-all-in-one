/**
 * 解析限额输入：支持纯数字或 K/M 后缀（大小写均可，如 `500K`、`2M`、`1.5m`）。
 * 非法或结果不是正数时返回 null（清除限额走专门按钮，不靠输入 0）。
 */
export function parseTokenAmount(input: string): number | null {
  const match = /^\s*(\d+(?:\.\d+)?)\s*([kKmM]?)\s*$/.exec(input)
  if (!match) return null
  const base = Number(match[1])
  const suffix = match[2].toLowerCase()
  const multiplier = suffix === 'k' ? 1_000 : suffix === 'm' ? 1_000_000 : 1
  const value = Math.round(base * multiplier)
  return value > 0 ? value : null
}

/** 限额进度条配色：>=100% 红，>=80% 橙，否则主色。 */
export function quotaBarClass(ratio: number): string {
  if (ratio >= 1) return 'bg-destructive'
  if (ratio >= 0.8) return 'bg-warning'
  return 'bg-primary'
}
