import type { RequestLogRow } from './types'

/** 后端 `UpstreamErrorKind::ContentFiltered` 的 error_kind 串:上游内容安全分类器拒答。 */
export const CONTENT_FILTERED_KIND = 'ContentFiltered'

/** 是否为上游分类器拦截的请求(零产出直接报错,或答到一半 stop_reason=refusal)。 */
export function isClassifierBlocked(row: Pick<RequestLogRow, 'error_kind'>): boolean {
  return row.error_kind === CONTENT_FILTERED_KIND
}

/**
 * 从 response_payload 取客户端实际收到的错误文案(`{"type":"error","error":{"message":...}}`)。
 * 不是错误体(成功回复/旧日志/空串)或解析失败 → null。
 */
export function clientErrorMessage(responsePayload: string): string | null {
  if (!responsePayload) return null
  try {
    const v: unknown = JSON.parse(responsePayload)
    if (typeof v !== 'object' || v === null) return null
    const body = v as { type?: unknown; error?: { message?: unknown } }
    if (body.type !== 'error') return null
    const msg = body.error?.message
    return typeof msg === 'string' && msg.length > 0 ? msg : null
  } catch {
    return null
  }
}
