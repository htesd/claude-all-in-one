import { forwardRef, type ButtonHTMLAttributes } from 'react'
import { cva, type VariantProps } from 'class-variance-authority'

import { cn } from '@/lib/utils'

const buttonVariants = cva(
  'inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-full text-sm font-semibold transition-all focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40 disabled:pointer-events-none disabled:opacity-50',
  {
    variants: {
      variant: {
        /* 主操作:黑 pill,hover 翻 acid(深色模式:奶白 pill,hover 翻 acid) */
        primary: 'bg-foreground text-background hover:bg-acid hover:text-ink dark:text-ink',
        outline: 'border border-border bg-card text-foreground hover:border-foreground/40',
        ghost:
          'text-muted-foreground hover:bg-black/5 hover:text-foreground dark:hover:bg-white/10',
        destructive:
          'bg-rose-600 text-white hover:bg-rose-500 dark:bg-rose-500/90 dark:hover:bg-rose-400',
      },
      size: {
        sm: 'h-8 px-3.5 text-xs',
        md: 'h-10 px-5',
        icon: 'h-9 w-9',
      },
    },
    defaultVariants: {
      variant: 'primary',
      size: 'md',
    },
  },
)

export interface ButtonProps
  extends ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, type = 'button', ...props }, ref) => (
    <button
      ref={ref}
      type={type}
      className={cn(buttonVariants({ variant, size }), className)}
      {...props}
    />
  ),
)
Button.displayName = 'Button'
