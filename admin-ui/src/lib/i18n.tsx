import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useState,
  type ReactNode,
} from 'react'

const LANG_STORAGE_KEY = 'kiroGwLang'

const zh = {
  'app.title': 'Kiro Gateway',
  'app.subtitle': '管理控制台',
  'nav.dashboard': '看板',
  'nav.usage': '用量',
  'nav.accounts': '账号',
  'nav.apiKeys': 'API Keys',
  'nav.groups': '分组',
  'nav.comingSoon': '即将上线',
  'nav.logout': '退出登录',
  'nav.collapse': '收起',
  'nav.expand': '展开侧边栏',
  'theme.toLight': '切换浅色模式',
  'theme.toDark': '切换深色模式',
  'login.title': 'Kiro Gateway',
  'login.subtitle': '输入管理员令牌以继续',
  'login.placeholder': '管理员令牌',
  'login.submit': '进入',
  'login.checking': '验证中…',
  'login.invalid': '令牌无效或无管理权限',
  'range.7d': '7 天',
  'range.30d': '30 天',
  'range.all': '全部',
  'stats.requests': '总请求',
  'stats.successRate': '成功率',
  'stats.inputTokens': '输入 Tokens',
  'stats.outputTokens': '输出 Tokens',
  'stats.cacheRead': '缓存读 Tokens',
  'stats.cacheWrite': '缓存写 Tokens',
  'dashboard.title': '看板',
  'dashboard.subtitle': '网关运行与用量概览',
  'usage.title': '用量统计',
  'usage.subtitle': '按模型与 API Key 的用量明细',
  'table.byModel': '按模型统计',
  'table.byKey': '按 API Key 统计',
  'table.model': '模型',
  'table.key': 'API Key',
  'table.requests': '请求数',
  'table.success': '成功率',
  'table.input': '输入',
  'table.output': '输出',
  'table.cacheRead': '缓存读',
  'table.cacheWrite': '缓存写',
  'table.unattributed': '未归属',
  'table.empty': '暂无数据',
  'chart.topModels': '模型请求 Top 8',
  'common.loadFailed': '加载失败',
} as const

export type I18nKey = keyof typeof zh
export type Lang = 'zh' | 'en'

const en: Record<I18nKey, string> = {
  'app.title': 'Kiro Gateway',
  'app.subtitle': 'Admin Console',
  'nav.dashboard': 'Dashboard',
  'nav.usage': 'Usage',
  'nav.accounts': 'Accounts',
  'nav.apiKeys': 'API Keys',
  'nav.groups': 'Groups',
  'nav.comingSoon': 'Coming soon',
  'nav.logout': 'Log out',
  'nav.collapse': 'Collapse',
  'nav.expand': 'Expand sidebar',
  'theme.toLight': 'Switch to light mode',
  'theme.toDark': 'Switch to dark mode',
  'login.title': 'Kiro Gateway',
  'login.subtitle': 'Enter the admin token to continue',
  'login.placeholder': 'Admin token',
  'login.submit': 'Enter',
  'login.checking': 'Verifying…',
  'login.invalid': 'Invalid token or no admin permission',
  'range.7d': '7d',
  'range.30d': '30d',
  'range.all': 'All',
  'stats.requests': 'Requests',
  'stats.successRate': 'Success rate',
  'stats.inputTokens': 'Input tokens',
  'stats.outputTokens': 'Output tokens',
  'stats.cacheRead': 'Cache read',
  'stats.cacheWrite': 'Cache write',
  'dashboard.title': 'Dashboard',
  'dashboard.subtitle': 'Gateway health and usage at a glance',
  'usage.title': 'Usage',
  'usage.subtitle': 'Usage breakdown by model and API key',
  'table.byModel': 'By model',
  'table.byKey': 'By API key',
  'table.model': 'Model',
  'table.key': 'API Key',
  'table.requests': 'Requests',
  'table.success': 'Success',
  'table.input': 'Input',
  'table.output': 'Output',
  'table.cacheRead': 'Cache read',
  'table.cacheWrite': 'Cache write',
  'table.unattributed': 'Unattributed',
  'table.empty': 'No data yet',
  'chart.topModels': 'Top 8 models by requests',
  'common.loadFailed': 'Failed to load',
}

const dictionaries: Record<Lang, Record<I18nKey, string>> = { zh, en }

interface I18nContextValue {
  lang: Lang
  setLang: (lang: Lang) => void
  t: (key: I18nKey) => string
}

const I18nContext = createContext<I18nContextValue | null>(null)

function readInitialLang(): Lang {
  try {
    return localStorage.getItem(LANG_STORAGE_KEY) === 'en' ? 'en' : 'zh'
  } catch {
    return 'zh'
  }
}

export function I18nProvider({ children }: { children: ReactNode }) {
  const [lang, setLangState] = useState<Lang>(readInitialLang)

  const setLang = useCallback((next: Lang) => {
    try {
      localStorage.setItem(LANG_STORAGE_KEY, next)
    } catch {
      /* ignore storage errors */
    }
    setLangState(next)
  }, [])

  const t = useCallback((key: I18nKey) => dictionaries[lang][key] ?? zh[key], [lang])

  const value = useMemo(() => ({ lang, setLang, t }), [lang, setLang, t])

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>
}

export function useI18n(): I18nContextValue {
  const ctx = useContext(I18nContext)
  if (!ctx) {
    throw new Error('useI18n must be used within an I18nProvider')
  }
  return ctx
}
