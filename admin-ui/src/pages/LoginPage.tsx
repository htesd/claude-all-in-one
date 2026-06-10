import { useState, type FormEvent } from 'react'
import { KeyRound, Loader2, LogIn, ShieldCheck } from 'lucide-react'
import { useNavigate } from 'react-router-dom'

import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import {
  clearToken,
  extractErrorMessage,
  isUnauthorizedError,
  pingAdmin,
  setToken,
} from '@/lib/api'
import { useI18n } from '@/lib/i18n'

export default function LoginPage() {
  const { t } = useI18n()
  const navigate = useNavigate()
  const [tokenInput, setTokenInput] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    const token = tokenInput.trim()
    if (!token || loading) return

    setLoading(true)
    setError(null)
    try {
      // setToken 在 try 内:隐私模式等场景 sessionStorage 可能抛异常,
      // 放外面会跳过 finally 把按钮卡死在 loading。
      setToken(token)
      const ok = await pingAdmin(token)
      if (ok) {
        navigate('/', { replace: true })
      } else {
        clearToken()
        setError(t('login.invalid'))
      }
    } catch (err) {
      clearToken()
      setError(isUnauthorizedError(err) ? t('login.invalid') : extractErrorMessage(err))
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="ambient-bg flex min-h-screen items-center justify-center p-4">
      <Card variant="glass-strong" className="acrylic-noise w-full max-w-sm p-8">
        <div className="flex flex-col items-center text-center">
          <div className="gradient-bg-primary breathe-glow flex h-14 w-14 items-center justify-center rounded-2xl shadow-lg">
            <ShieldCheck className="h-7 w-7 text-white" />
          </div>
          <h1 className="mt-4 text-xl font-bold tracking-tight">{t('login.title')}</h1>
          <p className="mt-1 text-sm text-muted-foreground">{t('login.subtitle')}</p>
        </div>

        <form onSubmit={handleSubmit} className="mt-6 space-y-4">
          <div className="relative">
            <KeyRound className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <input
              type="password"
              value={tokenInput}
              onChange={(event) => {
                setTokenInput(event.target.value)
                if (error) setError(null)
              }}
              placeholder={t('login.placeholder')}
              autoFocus
              autoComplete="current-password"
              className="w-full rounded-xl border bg-input py-2.5 pl-10 pr-4 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground"
            />
          </div>

          {error && <p className="text-sm text-destructive">{error}</p>}

          <Button type="submit" className="w-full" disabled={!tokenInput.trim() || loading}>
            {loading ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <LogIn className="h-4 w-4" />
            )}
            {loading ? t('login.checking') : t('login.submit')}
          </Button>
        </form>
      </Card>
    </div>
  )
}
