import type { HeaderOverride } from './headers'
import { isObject, request } from '../request'

/** 本插件真实出站处理器记录的请求头前后对照 */
export interface OutboundEvidence {
  accountId: string
  requestId: string
  attemptIndex: number
  transport: string
  overrideEnabled: boolean
  beforeHeaders: HeaderOverride[]
  headers: HeaderOverride[]
}

/** 普通模型请求的真实结果与同一请求的插件记录 */
export interface TestResult {
  status: 'success' | 'failed' | 'unverified'
  message?: string
  text?: string
  outbound: OutboundEvidence[]
  error?: { upstreamStatus: number, upstreamBody: string }
}

/**
 * 由独立检测插件调用宿主定向模型接口，原请求头插件参与真实出站处理
 * @param id 随机检测编号
 * @param accountId 所选账号
 * @param modelId 模型标识
 * @param signal 页面取消与期限
 * @return 模型结果及真实覆写记录
 */
export async function testAccount(id: string, accountId: string, modelId: string, signal: AbortSignal): Promise<TestResult> {
  try {
    const value = await abortable(request('test', new URLSearchParams(), { id, accountId, modelId }), signal)
    if (!isObject(value) || !['success', 'failed', 'unverified'].includes(String(value.status))
      || !Array.isArray(value.outbound) || !value.outbound.every(item => isEvidence(item, accountId, value.requestId))) {
      throw new Error('检测结果响应无效')
    }
    return value as unknown as TestResult
  }
  finally {
    if (signal.aborted)
      await request('test/cancel', new URLSearchParams(), { id }).catch(() => {})
  }
}

function isEvidence(value: unknown, accountId: string, requestId: unknown): value is OutboundEvidence {
  const headers = (items: unknown) => Array.isArray(items) && items.every(item => isObject(item) && typeof item.name === 'string' && typeof item.value === 'string')
  return isObject(value) && value.accountId === accountId && value.requestId === requestId
    && typeof value.attemptIndex === 'number' && typeof value.transport === 'string' && typeof value.overrideEnabled === 'boolean'
    && headers(value.beforeHeaders) && headers(value.headers)
}

/** 账号公开模型选择项 */
export interface AccountModel { id: string, label: string }
/**
 * 读取所选账号的实际模型目录
 * @param accountId 目标账号
 * @return 可搜索模型选择项
 */
export async function getAccountModels(accountId: string): Promise<AccountModel[]> {
  const value = await request('models', new URLSearchParams({ accountId }))
  if (!isObject(value) || !Array.isArray(value.models) || !value.models.every((item: unknown): item is AccountModel => isObject(item) && typeof item.id === 'string' && typeof item.label === 'string'))
    throw new Error('账号模型列表响应无效')
  return value.models
}

async function abortable<T>(operation: Promise<T>, signal: AbortSignal): Promise<T> {
  if (signal.aborted)
    throw new DOMException('测试已停止', 'AbortError')
  let onAbort: () => void = () => {}
  const cancelled = new Promise<never>((_, reject) => {
    onAbort = () => reject(new DOMException('测试已停止', 'AbortError'))
    signal.addEventListener('abort', onAbort, { once: true })
  })
  try {
    return await Promise.race([operation, cancelled])
  }
  finally { signal.removeEventListener('abort', onAbort) }
}

/** 当前插件实例的统一测试设置 */
export interface SharedTestSettings { groupId: string | null, version: string | null }
/** 可选择的原生分组 */
export interface TestGroup { id: string, name: string, enabled: boolean }

/**
 * 读取统一设置及已有分组
 * @return 设置版本与当前分组选择项
 */
export async function getTestSettings(): Promise<{ settings: SharedTestSettings, groups: TestGroup[] }> {
  const value = await request('test/settings')
  if (!isObject(value) || !isSettings(value.settings) || !Array.isArray(value.groups)
    || !value.groups.every((item: unknown): item is TestGroup => isObject(item) && typeof item.id === 'string' && typeof item.name === 'string' && typeof item.enabled === 'boolean')) {
    throw new Error('测试设置响应无效')
  }
  return { settings: value.settings, groups: value.groups }
}

/**
 * 按读取版本保存统一测试设置
 * @param settings 所选分组与精确版本，空分组使用自动模式
 * @return 已确认的持久化设置
 */
export async function saveTestSettings(settings: SharedTestSettings): Promise<SharedTestSettings> {
  const value = await request('test/settings', new URLSearchParams(), settings)
  if (!isSettings(value))
    throw new Error('保存结果待确认，请重新读取')
  return value
}

function isSettings(value: unknown): value is SharedTestSettings {
  return isObject(value) && (value.groupId === null || typeof value.groupId === 'string')
    && (value.version === null || typeof value.version === 'string')
}
