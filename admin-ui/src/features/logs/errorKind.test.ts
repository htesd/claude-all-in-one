import { describe, expect, it } from 'vitest'

import { clientErrorMessage, isClassifierBlocked } from './errorKind'

describe('isClassifierBlocked', () => {
  it('只认 ContentFiltered', () => {
    expect(isClassifierBlocked({ error_kind: 'ContentFiltered' })).toBe(true)
    expect(isClassifierBlocked({ error_kind: 'EmptyResponse' })).toBe(false)
    expect(isClassifierBlocked({ error_kind: null })).toBe(false)
  })
})

describe('clientErrorMessage', () => {
  it('取出错误体里的 message', () => {
    const body = JSON.stringify({
      type: 'error',
      error: { type: 'invalid_request_error', message: '请求被模型的内容安全分类器拒绝(类别:REASONING_EXTRACTION)' },
    })
    expect(clientErrorMessage(body)).toContain('REASONING_EXTRACTION')
  })

  it('成功回复、空串、坏 JSON 一律 null', () => {
    expect(clientErrorMessage(JSON.stringify({ type: 'message', stop_reason: 'refusal' }))).toBeNull()
    expect(clientErrorMessage('')).toBeNull()
    expect(clientErrorMessage('{not json')).toBeNull()
    expect(clientErrorMessage(JSON.stringify({ type: 'error', error: {} }))).toBeNull()
  })
})
