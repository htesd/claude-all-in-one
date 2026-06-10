import { useCallback, useState } from 'react'

const THEME_STORAGE_KEY = 'caioTheme'

/**
 * Dark/light theme hook. The initial `.dark` class is applied by an inline
 * script in index.html (before first paint); this hook just mirrors and
 * toggles that state.
 */
export function useTheme() {
  const [dark, setDark] = useState<boolean>(() =>
    document.documentElement.classList.contains('dark'),
  )

  const toggleTheme = useCallback(() => {
    setDark((prev) => {
      const next = !prev
      document.documentElement.classList.toggle('dark', next)
      try {
        localStorage.setItem(THEME_STORAGE_KEY, next ? 'dark' : 'light')
      } catch {
        /* ignore storage errors */
      }
      return next
    })
  }, [])

  return { dark, toggleTheme }
}
