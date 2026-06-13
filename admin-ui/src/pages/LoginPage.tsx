import { useState, type FormEvent } from 'react'
import { KeyRound, Loader2, LogIn } from 'lucide-react'
import { useNavigate } from 'react-router-dom'

import {
  clearToken,
  extractErrorMessage,
  isUnauthorizedError,
  pingAdmin,
  setToken,
} from '@/lib/api'
import { useI18n } from '@/lib/i18n'

/** 登录页是品牌时刻:无论明暗模式,恒为 ink 暗色画布 + acid 强调。 */
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
      <div className="w-full max-w-sm rounded-[2.5rem] border border-white/10 bg-white/[0.08] p-8 shadow-2xl shadow-black/35 backdrop-blur-xl">
        <div className="text-center">
          <p className="text-[11px] font-semibold uppercase tracking-[0.32em] text-white/35">
            Admin Console
          </p>
          <h1 className="mt-3 font-display text-5xl font-black leading-none tracking-[-0.06em] text-white">
            CA<span className="text-acid">IO</span>
          </h1>
          <p className="mt-3 text-sm text-white/55">{t('login.subtitle')}</p>
        </div>

        <form onSubmit={handleSubmit} className="mt-8 space-y-4">
          <div className="relative">
            <KeyRound className="pointer-events-none absolute left-4 top-1/2 h-4 w-4 -translate-y-1/2 text-white/30" />
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
              className="w-full rounded-2xl border bg-black/35 py-3 pl-11 pr-4 text-sm text-white outline-none transition-colors placeholder:text-white/25 !border-white/10 focus:!border-acid"
            />
          </div>

          {error && <p className="text-sm text-rose-300">{error}</p>}

          <button
            type="submit"
            disabled={!tokenInput.trim() || loading}
            className="inline-flex w-full items-center justify-center gap-2 rounded-full bg-acid py-3 text-sm font-semibold text-ink transition hover:bg-white disabled:pointer-events-none disabled:opacity-50"
          >
            {loading ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <LogIn className="h-4 w-4" />
            )}
            {loading ? t('login.checking') : t('login.submit')}
          </button>
        </form>
      </div>
    </div>
  )
}
