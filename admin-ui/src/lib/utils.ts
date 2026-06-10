import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs))
}

/**
 * Mask an API key for display: head4 + *** + tail2.
 * Keys with length <= 8 are fully hidden.
 */
export function maskKey(key: string): string {
  if (!key) return ''
  if (key.length <= 8) return '********'
  return `${key.slice(0, 4)}***${key.slice(-2)}`
}

export function formatInt(value: number): string {
  return Math.round(value).toLocaleString('en-US')
}

export function formatCompact(value: number): string {
  return new Intl.NumberFormat('en-US', {
    notation: 'compact',
    maximumFractionDigits: 1,
  }).format(value)
}

export function formatPercent(numerator: number, denominator: number): string {
  if (!denominator) return '—'
  return `${((numerator / denominator) * 100).toFixed(1)}%`
}

export function truncate(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text
}
