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
  'keys.title': 'API Keys',
  'keys.subtitle': '管理客户端 API Key：创建、备注、启停与删除',
  'keys.new': '新建 Key',
  'keys.listTitle': 'Key 列表',
  'keys.empty': '还没有 API Key，点击右上角「新建 Key」创建第一个',
  'keys.status.enabled': '启用',
  'keys.status.disabled': '禁用',
  'keys.action.enable': '启用',
  'keys.action.disable': '禁用',
  'keys.action.delete': '删除',
  'keys.action.editLabel': '编辑备注',
  'keys.action.copy': '复制完整 Key',
  'keys.action.copied': '已复制',
  'keys.action.copyFailed': '复制失败，请手动复制',
  'keys.label.placeholder': '备注（可选）',
  'keys.label.save': '保存',
  'keys.delete.hint': '历史用量记录会保留',
  'keys.delete.confirm': '确认删除',
  'keys.create.title': '新建 API Key',
  'keys.create.label': '备注',
  'keys.create.labelPlaceholder': '例如：测试环境',
  'keys.create.customToggle': '自定义 Key（可选）',
  'keys.create.customPlaceholder': '留空则自动生成 sk-gw-…',
  'keys.create.customRule': '8–128 个字符，仅限字母、数字和 - _ . ~',
  'keys.create.submit': '创建',
  'keys.create.creating': '创建中…',
  'keys.create.successTitle': 'Key 创建成功',
  'keys.create.successHint': '请妥善保存此 Key，避免泄露给无关人员',
  'keys.create.done': '完成',
  'keys.error.duplicate': '该 Key 已存在',
  'keys.error.invalidKey': 'Key 格式不合法：需 8–128 个字符，仅限字母、数字和 - _ . ~',
  'keys.usageLoadFailed': '用量数据加载失败，以下用量列可能不准确',
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
  'table.label': '备注',
  'table.status': '状态',
  'table.createdAt': '创建时间',
  'table.tokens': 'Token 合计',
  'table.actions': '操作',
  'filter.from': '起',
  'filter.to': '止',
  'filter.clear': '清除',
  'filter.allKeys': '全部 Key',
  'common.loadFailed': '加载失败',
  'common.actionFailed': '操作失败',
  'common.cancel': '取消',
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
  'keys.title': 'API Keys',
  'keys.subtitle': 'Manage client API keys: create, label, enable/disable, delete',
  'keys.new': 'New Key',
  'keys.listTitle': 'Keys',
  'keys.empty': 'No API keys yet — click "New Key" in the top right to create one',
  'keys.status.enabled': 'Enabled',
  'keys.status.disabled': 'Disabled',
  'keys.action.enable': 'Enable',
  'keys.action.disable': 'Disable',
  'keys.action.delete': 'Delete',
  'keys.action.editLabel': 'Edit label',
  'keys.action.copy': 'Copy full key',
  'keys.action.copied': 'Copied',
  'keys.action.copyFailed': 'Copy failed, please copy manually',
  'keys.label.placeholder': 'Label (optional)',
  'keys.label.save': 'Save',
  'keys.delete.hint': 'Usage history will be kept',
  'keys.delete.confirm': 'Confirm delete',
  'keys.create.title': 'New API Key',
  'keys.create.label': 'Label',
  'keys.create.labelPlaceholder': 'e.g. staging',
  'keys.create.customToggle': 'Custom key (optional)',
  'keys.create.customPlaceholder': 'Leave blank to auto-generate sk-gw-…',
  'keys.create.customRule': '8–128 characters: letters, digits and - _ . ~ only',
  'keys.create.submit': 'Create',
  'keys.create.creating': 'Creating…',
  'keys.create.successTitle': 'Key created',
  'keys.create.successHint': 'Store this key safely and do not share it',
  'keys.create.done': 'Done',
  'keys.error.duplicate': 'This key already exists',
  'keys.error.invalidKey': 'Invalid key: must be 8–128 characters using only letters, digits and - _ . ~',
  'keys.usageLoadFailed': 'Failed to load usage data — the usage columns below may be inaccurate',
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
  'table.label': 'Label',
  'table.status': 'Status',
  'table.createdAt': 'Created',
  'table.tokens': 'Tokens',
  'table.actions': 'Actions',
  'filter.from': 'From',
  'filter.to': 'To',
  'filter.clear': 'Clear',
  'filter.allKeys': 'All keys',
  'common.loadFailed': 'Failed to load',
  'common.actionFailed': 'Action failed',
  'common.cancel': 'Cancel',
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
