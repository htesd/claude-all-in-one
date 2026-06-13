import { forwardRef, type HTMLAttributes } from 'react'

import { cn } from '@/lib/utils'

export type CardVariant = 'glass' | 'glass-strong' | 'glass-subtle' | 'solid'

interface CardProps extends HTMLAttributes<HTMLDivElement> {
  /**
   * 变体名沿用旧 API(glass-*),视觉已切换为实色纸面卡片:
   * - glass        : 默认白卡(实色 + 轻边框 + 微阴影)
   * - glass-strong : 弹窗级卡片(实色 + 远投影)
   * - glass-subtle : 二级容器(米白面板)
   * - solid        : 同 glass(兼容保留)
   */
  variant?: CardVariant
  /** 是否启用 hover 浮起动画 */
  interactive?: boolean
}

const variantClass: Record<CardVariant, string> = {
  glass: 'glass-card text-card-foreground',
  'glass-strong': 'glass-card-strong text-card-foreground',
  'glass-subtle': 'glass-card-subtle text-card-foreground',
  solid: 'glass-card text-card-foreground',
}

export const Card = forwardRef<HTMLDivElement, CardProps>(
  ({ className, variant = 'glass', interactive = false, ...props }, ref) => (
    <div
      ref={ref}
      className={cn(
        'rounded-2xl',
        variantClass[variant],
        interactive && 'hover-lift cursor-pointer',
        className,
      )}
      {...props}
    />
  ),
)
Card.displayName = 'Card'

export const CardHeader = forwardRef<HTMLDivElement, HTMLAttributes<HTMLDivElement>>(
  ({ className, ...props }, ref) => (
    <div ref={ref} className={cn('flex flex-col space-y-1.5 p-6', className)} {...props} />
  ),
)
CardHeader.displayName = 'CardHeader'

export const CardTitle = forwardRef<HTMLDivElement, HTMLAttributes<HTMLDivElement>>(
  ({ className, ...props }, ref) => (
    <div
      ref={ref}
      className={cn('font-black leading-none tracking-[-0.02em]', className)}
      {...props}
    />
  ),
)
CardTitle.displayName = 'CardTitle'

export const CardDescription = forwardRef<HTMLDivElement, HTMLAttributes<HTMLDivElement>>(
  ({ className, ...props }, ref) => (
    <div ref={ref} className={cn('text-sm text-muted-foreground', className)} {...props} />
  ),
)
CardDescription.displayName = 'CardDescription'

export const CardContent = forwardRef<HTMLDivElement, HTMLAttributes<HTMLDivElement>>(
  ({ className, ...props }, ref) => (
    <div ref={ref} className={cn('p-6 pt-0', className)} {...props} />
  ),
)
CardContent.displayName = 'CardContent'
