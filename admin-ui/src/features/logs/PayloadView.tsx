import { useMemo } from 'react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'

import { Badge } from '@/components/ui/badge'
import { cn } from '@/lib/utils'

import type { LogBlob } from './types'

/** 报文展示模式:格式化(解析成对话 + Markdown 渲染) / 原始(美化 JSON)。 */
export type PayloadMode = 'formatted' | 'raw'

/** 单个 text block 超此长度不跑 Markdown,改纯文本 <pre>——防不可信超大正文卡死详情页主线程。 */
const MAX_MARKDOWN_CHARS = 20_000

// ── Markdown 渲染 ───────────────────────────────────────────────────────────
// react-markdown 默认**不**渲染原始 HTML(未引入 rehype-raw),且默认 urlTransform 拦截
// javascript: 等危险协议,故日志内容无 XSS 风险。无 typography 插件,关键元素手配 tailwind 类。
const MD_COMPONENTS: Components = {
  p: ({ children }) => <p className="my-1.5 first:mt-0 last:mb-0">{children}</p>,
  a: ({ children, href }) => (
    <a href={href} target="_blank" rel="noreferrer" className="text-acid underline underline-offset-2">
      {children}
    </a>
  ),
  ul: ({ children }) => <ul className="my-1.5 list-disc pl-5">{children}</ul>,
  ol: ({ children }) => <ol className="my-1.5 list-decimal pl-5">{children}</ol>,
  li: ({ children }) => <li className="my-0.5">{children}</li>,
  h1: ({ children }) => <h1 className="mt-3 mb-1.5 text-base font-bold">{children}</h1>,
  h2: ({ children }) => <h2 className="mt-3 mb-1.5 text-sm font-bold">{children}</h2>,
  h3: ({ children }) => <h3 className="mt-2 mb-1 text-sm font-semibold">{children}</h3>,
  blockquote: ({ children }) => (
    <blockquote className="my-2 border-l-2 border-border pl-3 text-muted-foreground">{children}</blockquote>
  ),
  pre: ({ children }) => (
    <pre className="my-2 overflow-auto rounded-xl bg-ink/90 p-3 text-xs leading-relaxed text-white/90">
      {children}
    </pre>
  ),
  code: ({ className, children }) => {
    const isBlock = /language-/.test(className ?? '') || String(children).includes('\n')
    if (isBlock) {
      return <code className={cn('font-mono text-xs', className)}>{children}</code>
    }
    return (
      <code className="rounded bg-black/10 px-1 py-0.5 font-mono text-[0.85em] dark:bg-white/10">
        {children}
      </code>
    )
  },
  // 日志正文为不可信客户输入:不加载其中 Markdown 图片的远程 URL(防追踪像素/SSRF),
  // 只显示占位 + alt 文本。(用户上传的真实图片走 blob 通道单独渲染,见 MediaView。)
  img: ({ alt }) => (
    <span className="text-xs italic text-muted-foreground">[图片{alt ? `: ${alt}` : ''}]</span>
  ),
  table: ({ children }) => (
    <div className="my-2 overflow-auto">
      <table className="w-full border-collapse text-xs">{children}</table>
    </div>
  ),
  th: ({ children }) => <th className="border border-border px-2 py-1 text-left font-semibold">{children}</th>,
  td: ({ children }) => <td className="border border-border px-2 py-1">{children}</td>,
}

function Markdown({ children }: { children: string }) {
  // 超大正文不跑 Markdown 解析/建树:直接纯文本展示,避免主线程卡死(审查 Skeptic#5)。
  if (children.length > MAX_MARKDOWN_CHARS) {
    return (
      <pre className="overflow-auto rounded-xl bg-black/5 p-3 text-xs leading-relaxed whitespace-pre-wrap break-words dark:bg-white/5">
        {children}
      </pre>
    )
  }
  return (
    <div className="text-sm leading-relaxed break-words">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={MD_COMPONENTS}>
        {children}
      </ReactMarkdown>
    </div>
  )
}

// ── 归一化的对话模型 ─────────────────────────────────────────────────────────
type Block =
  | { kind: 'text'; text: string }
  | { kind: 'thinking'; text: string }
  | { kind: 'tool_use'; name: string; input: unknown; id?: string }
  | { kind: 'tool_result'; id?: string; text: string; isError?: boolean }
  | { kind: 'media'; hash?: string; mediaType: string; label: string }

interface Turn {
  role: string
  blocks: Block[]
}

interface Conversation {
  meta: { label: string; value: string }[]
  turns: Turn[]
}

function asText(v: unknown): string {
  if (typeof v === 'string') return v
  if (v == null) return ''
  return JSON.stringify(v, null, 2)
}

/** 若字符串是 `blob:<hash>` 媒体引用,返回 hash,否则 null。 */
function blobHashOf(v: unknown): string | null {
  return typeof v === 'string' && v.startsWith('blob:') ? v.slice(5) : null
}

/** 把 Anthropic content(string | block[])里的 tool_result 内容拍平成纯文本。 */
function flattenToolResultContent(content: unknown): string {
  if (typeof content === 'string') return content
  if (Array.isArray(content)) {
    return content
      .map((c) => {
        if (c && typeof c === 'object' && 'text' in c) return asText((c as { text: unknown }).text)
        if (c && typeof c === 'object' && (c as { type?: string }).type === 'image') return '[图片]'
        return asText(c)
      })
      .join('\n')
  }
  return asText(content)
}

/** image/document 块 → media Block(带 blob hash 与 media_type,供 MediaView 渲染)。 */
function mediaBlockFromAnthropic(b: Record<string, unknown>): Block {
  const source = (b.source ?? {}) as Record<string, unknown>
  const hash = blobHashOf(source.data) ?? undefined
  const mediaType = asText(source.media_type)
  const label = b.type === 'document' ? '文档' : '图片'
  return { kind: 'media', hash, mediaType, label }
}

/** Anthropic content 块 → 归一化 blocks。 */
function anthropicBlocks(content: unknown): Block[] {
  if (typeof content === 'string') return [{ kind: 'text', text: content }]
  if (!Array.isArray(content)) return [{ kind: 'text', text: asText(content) }]
  const out: Block[] = []
  for (const raw of content) {
    if (!raw || typeof raw !== 'object') {
      out.push({ kind: 'text', text: asText(raw) })
      continue
    }
    const b = raw as Record<string, unknown>
    switch (b.type) {
      case 'text':
        out.push({ kind: 'text', text: asText(b.text) })
        break
      case 'thinking':
      case 'redacted_thinking':
        out.push({ kind: 'thinking', text: asText(b.thinking ?? b.text ?? '[已加密思考]') })
        break
      case 'tool_use':
        out.push({ kind: 'tool_use', name: asText(b.name), input: b.input, id: b.id as string })
        break
      case 'tool_result':
        out.push({
          kind: 'tool_result',
          id: b.tool_use_id as string,
          text: flattenToolResultContent(b.content),
          isError: b.is_error === true,
        })
        break
      case 'image':
      case 'document':
        out.push(mediaBlockFromAnthropic(b))
        break
      default:
        out.push({ kind: 'text', text: asText(b.text ?? raw) })
    }
  }
  return out
}

/** 解析 Anthropic 请求体。非该格式返回 null。 */
function parseAnthropic(obj: Record<string, unknown>): Conversation | null {
  if (!Array.isArray(obj.messages)) return null
  const turns: Turn[] = []
  if (obj.system != null) {
    const sys = Array.isArray(obj.system)
      ? obj.system
          .map((s) => (s && typeof s === 'object' && 'text' in s ? asText((s as { text: unknown }).text) : asText(s)))
          .join('\n')
      : asText(obj.system)
    if (sys.trim()) turns.push({ role: 'system', blocks: [{ kind: 'text', text: sys }] })
  }
  for (const m of obj.messages) {
    if (!m || typeof m !== 'object') {
      turns.push({ role: 'user', blocks: [{ kind: 'text', text: asText(m) }] })
      continue
    }
    const msg = m as Record<string, unknown>
    turns.push({ role: asText(msg.role) || 'user', blocks: anthropicBlocks(msg.content) })
  }
  const meta: Conversation['meta'] = []
  if (typeof obj.model === 'string') meta.push({ label: 'model', value: obj.model })
  if (Array.isArray(obj.tools)) {
    const names = obj.tools
      .map((t) => (t && typeof t === 'object' ? asText((t as { name?: unknown }).name) : ''))
      .filter(Boolean)
    if (names.length) meta.push({ label: 'tools', value: names.join(', ') })
  }
  return { meta, turns }
}

/** Kiro userInputMessage / assistantResponseMessage → 归一化 Turn。 */
function kiroMessageToTurn(node: unknown): Turn | null {
  if (!node || typeof node !== 'object') return null
  const n = node as Record<string, unknown>
  const user = n.userInputMessage as Record<string, unknown> | undefined
  if (user && typeof user === 'object') {
    const blocks: Block[] = []
    if (asText(user.content).trim()) blocks.push({ kind: 'text', text: asText(user.content) })
    const ctx = user.userInputMessageContext as Record<string, unknown> | undefined
    const toolResults = ctx && Array.isArray(ctx.toolResults) ? ctx.toolResults : []
    for (const tr of toolResults) {
      if (!tr || typeof tr !== 'object') continue
      const t = tr as Record<string, unknown>
      blocks.push({
        kind: 'tool_result',
        id: asText(t.toolUseId),
        text: flattenToolResultContent(t.content),
        isError: asText(t.status) === 'error',
      })
    }
    if (Array.isArray(user.images)) {
      for (const img of user.images) {
        if (!img || typeof img !== 'object') continue
        const im = img as Record<string, unknown>
        const source = (im.source ?? {}) as Record<string, unknown>
        const hash = blobHashOf(source.bytes) ?? undefined
        const fmt = asText(im.format)
        blocks.push({ kind: 'media', hash, mediaType: fmt ? `image/${fmt}` : '', label: '图片' })
      }
    }
    return { role: 'user', blocks }
  }
  const asst = n.assistantResponseMessage as Record<string, unknown> | undefined
  if (asst && typeof asst === 'object') {
    const blocks: Block[] = []
    if (asText(asst.content).trim()) blocks.push({ kind: 'text', text: asText(asst.content) })
    const toolUses = Array.isArray(asst.toolUses) ? asst.toolUses : []
    for (const tu of toolUses) {
      if (!tu || typeof tu !== 'object') continue
      const t = tu as Record<string, unknown>
      blocks.push({ kind: 'tool_use', name: asText(t.name), input: t.input, id: asText(t.toolUseId) })
    }
    return { role: 'assistant', blocks }
  }
  return null
}

/** 解析 Kiro 线缆报文(conversationState)。非该格式返回 null。 */
function parseKiro(obj: Record<string, unknown>): Conversation | null {
  const cs = obj.conversationState as Record<string, unknown> | undefined
  if (!cs || typeof cs !== 'object') return null
  const turns: Turn[] = []
  if (Array.isArray(cs.history)) {
    for (const h of cs.history) {
      const t = kiroMessageToTurn(h)
      if (t) turns.push(t)
    }
  }
  const cur = kiroMessageToTurn(cs.currentMessage)
  if (cur) turns.push(cur)
  const meta: Conversation['meta'] = []
  if (typeof cs.conversationId === 'string') meta.push({ label: 'conversationId', value: cs.conversationId })
  if (typeof cs.chatTriggerType === 'string') meta.push({ label: 'trigger', value: cs.chatTriggerType })
  return { meta, turns }
}

function parseConversation(raw: string): Conversation | null {
  try {
    const obj: unknown = JSON.parse(raw)
    if (!obj || typeof obj !== 'object') return null
    return parseAnthropic(obj as Record<string, unknown>) ?? parseKiro(obj as Record<string, unknown>)
  } catch {
    return null
  }
}

// ── 媒体渲染 ─────────────────────────────────────────────────────────────────
/** base64 magic 字节兜底推断 MIME(Kiro 线缆 blob 无 media_type 时)。 */
function sniffMediaType(data: string): string {
  if (data.startsWith('iVBOR')) return 'image/png'
  if (data.startsWith('/9j/')) return 'image/jpeg'
  if (data.startsWith('R0lGOD')) return 'image/gif'
  if (data.startsWith('UklGR')) return 'image/webp'
  if (data.startsWith('JVBER')) return 'application/pdf'
  return ''
}

// 安全 MIME 白名单:media_type 是**客户可控**的不可信值。只对已知安全类型生成 data URI——
// 内联渲染的图片(<img> 不执行脚本)与可下载文档(PDF)。其余(如 text/html、svg)**不生成
// data URI**,仅占位,杜绝「data:text/html 下载链接被中键/Ctrl 点开新标签执行任意 JS」(审查 high)。
const SAFE_IMAGE_TYPES = new Set([
  'image/png',
  'image/jpeg',
  'image/gif',
  'image/webp',
  'image/avif',
  'image/bmp',
])
const SAFE_DOWNLOAD_TYPES = new Set(['application/pdf'])

function MediaView({ block, blobs }: { block: Extract<Block, { kind: 'media' }>; blobs: Map<string, LogBlob> }) {
  const blob = block.hash ? blobs.get(block.hash) : undefined
  if (!blob) {
    // 旧日志/URL 图等无 blob:仅占位。
    return <p className="text-xs italic text-muted-foreground">[{block.label}]</p>
  }
  const mediaType = block.mediaType || blob.media_type || sniffMediaType(blob.data) || 'application/octet-stream'
  // blob.bytes 是 base64 文本长度;原始体积 ≈ ×3/4。展示估算原始大小。
  const origKb = `${((blob.bytes * 3) / 4 / 1024).toFixed(1)} KB`

  if (SAFE_IMAGE_TYPES.has(mediaType)) {
    return (
      <div className="space-y-1">
        <img
          src={`data:${mediaType};base64,${blob.data}`}
          alt={block.label}
          className="max-h-80 max-w-full rounded-xl border border-border object-contain"
        />
        <p className="text-[10px] text-muted-foreground">
          {mediaType} · ~{origKb}
        </p>
      </div>
    )
  }
  if (SAFE_DOWNLOAD_TYPES.has(mediaType)) {
    return (
      <a
        href={`data:${mediaType};base64,${blob.data}`}
        download={`${block.label}.pdf`}
        className="inline-flex items-center gap-1.5 rounded-xl border border-border px-3 py-1.5 text-xs hover:bg-black/5 dark:hover:bg-white/5"
      >
        📄 {block.label} · {mediaType} · ~{origKb}(点击下载)
      </a>
    )
  }
  // 未知/不安全 MIME:不生成 data URI(原始视图仍可看 base64),仅占位标注类型。
  return (
    <p className="text-xs italic text-muted-foreground">
      [{block.label} · {mediaType} · ~{origKb}(未知类型,见原始视图)]
    </p>
  )
}

// ── 渲染 ─────────────────────────────────────────────────────────────────────
const ROLE_STYLE: Record<string, string> = {
  system: 'bg-amber-100 text-amber-700 dark:bg-amber-400/10 dark:text-amber-300',
  user: 'bg-sky-100 text-sky-700 dark:bg-sky-400/10 dark:text-sky-300',
  assistant: 'bg-emerald-100 text-emerald-700 dark:bg-emerald-400/10 dark:text-emerald-300',
}

function BlockView({ block, blobs }: { block: Block; blobs: Map<string, LogBlob> }) {
  switch (block.kind) {
    case 'text':
      return <Markdown>{block.text}</Markdown>
    case 'thinking':
      return (
        <div className="rounded-xl border border-dashed border-border bg-black/[0.03] px-3 py-2 dark:bg-white/[0.03]">
          <p className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">thinking</p>
          <Markdown>{block.text}</Markdown>
        </div>
      )
    case 'tool_use':
      return (
        <div className="rounded-xl border border-border px-3 py-2">
          <p className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
            tool_use · <span className="font-mono normal-case text-foreground">{block.name}</span>
          </p>
          <pre className="overflow-auto rounded-lg bg-ink/90 p-2 text-xs text-white/90">
            {typeof block.input === 'string' ? block.input : JSON.stringify(block.input, null, 2)}
          </pre>
        </div>
      )
    case 'tool_result':
      return (
        <div className={cn('rounded-xl border px-3 py-2', block.isError ? 'border-rose-300' : 'border-border')}>
          <p className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
            tool_result{block.isError ? ' · error' : ''}
          </p>
          <pre className="max-h-72 overflow-auto rounded-lg bg-black/5 p-2 text-xs whitespace-pre-wrap break-words dark:bg-white/5">
            {block.text}
          </pre>
        </div>
      )
    case 'media':
      return <MediaView block={block} blobs={blobs} />
  }
}

function ConversationView({ conv, blobs }: { conv: Conversation; blobs: Map<string, LogBlob> }) {
  return (
    <div className="space-y-3">
      {conv.meta.length > 0 && (
        <div className="flex flex-wrap gap-x-4 gap-y-1 text-[11px] text-muted-foreground">
          {conv.meta.map((m) => (
            <span key={m.label} className="break-all">
              <span className="font-semibold">{m.label}:</span> <span className="font-mono">{m.value}</span>
            </span>
          ))}
        </div>
      )}
      {conv.turns.map((turn, i) => (
        <div key={i} className="rounded-2xl border border-border/60 p-3">
          <Badge className={cn('mb-2', ROLE_STYLE[turn.role] ?? 'bg-black/5 dark:bg-white/5')}>{turn.role}</Badge>
          <div className="space-y-2">
            {turn.blocks.length === 0 ? (
              <p className="text-xs italic text-muted-foreground">(空)</p>
            ) : (
              turn.blocks.map((b, j) => <BlockView key={j} block={b} blobs={blobs} />)
            )}
          </div>
        </div>
      ))}
    </div>
  )
}

function prettyJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}

interface PayloadViewProps {
  raw: string
  mode: PayloadMode
  blobs?: LogBlob[]
}

/** 单份报文展示:formatted 解析成对话 + Markdown(图片/文档从 blob 渲染);raw 美化 JSON。
 *  formatted 解析失败(非 Anthropic/Kiro 结构,或已截断为非法 JSON)自动回退到 raw。 */
export function PayloadView({ raw, mode, blobs }: PayloadViewProps) {
  const conv = useMemo(() => (mode === 'formatted' ? parseConversation(raw) : null), [raw, mode])
  const blobMap = useMemo(() => new Map((blobs ?? []).map((b) => [b.hash, b])), [blobs])

  if (mode === 'formatted' && conv) {
    return (
      <div className="max-h-[28rem] overflow-auto rounded-2xl bg-black/5 p-3 dark:bg-white/5">
        <ConversationView conv={conv} blobs={blobMap} />
      </div>
    )
  }
  return (
    <pre className="max-h-[28rem] overflow-auto rounded-2xl bg-black/5 p-3 text-xs leading-relaxed whitespace-pre-wrap break-all dark:bg-white/5">
      {prettyJson(raw)}
    </pre>
  )
}
