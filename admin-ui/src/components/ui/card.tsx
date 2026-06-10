import { forwardRef, type HTMLAttributes } from 'react'

import { cn } from '@/lib/utils'

export type CardVariant = 'glass' | 'glass-strong' | 'glass-subtle' | 'solid'

interface CardProps extends HTMLAttributes<HTMLDivElement> {
  /**
   * - glass        : 默认玻璃态（半透明 + blur）
   * - glass-strong : 更不透明的玻璃（适合需要更高可读性的内容）
   * - glass-subtle : 轻玻璃（适合二级容器、工具栏）
   * - solid        : 不透明实色 Card
   */
  variant?: CardVariant
  /** 是否启用 hover 浮起动画 */
  interactive?: boolean
}

const variantClass: Record<CardVariant, string> = {
  glass: 'glass-card text-card-foreground',
  'glass-strong': 'glass-card-strong text-card-foreground',
  'glass-subtle': 'glass-card-subtle text-card-foreground',
  solid: 'bg-card text-card-foreground border shadow',
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
      className={cn('font-semibold leading-none tracking-tight', className)}
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
