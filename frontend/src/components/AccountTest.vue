<script setup lang="ts">
import type { Account } from '../api/modules/headers'
import type { AccountModel, OutboundEvidence, TestResult } from '../api/modules/testing'
import { ZButton, ZDialog, ZFormItem, ZInput, ZSwitch, ZTag } from '@codex-proxy/ui'
import { ZSelect } from '@codex-proxy/ui/select'
import { computed, onBeforeUnmount, shallowRef, watch } from 'vue'
import { getAccountModels, testAccount } from '../api/modules/testing'

const props = defineProps<{ account?: Account }>()
const open = defineModel<boolean>({ required: true })
const selectedModel = shallowRef<string>()
const customModel = shallowRef('')
const manualModel = shallowRef(false)
const models = shallowRef<AccountModel[]>([])
const modelsLoading = shallowRef(false)
const modelsError = shallowRef('')
const model = computed(() => manualModel.value
  ? customModel.value
  : selectedModel.value ?? '')
const modelOptions = computed(() => models.value.map(item => ({ value: item.id, label: item.label })))
const remembered = new Map<string, string>()
let catalogSequence = 0
const active = shallowRef('')
const stopping = shallowRef(false)
const result = shallowRef<TestResult & { elapsedMs: number } | { status: 'cancelled' | 'timeout', elapsedMs: number, outbound: OutboundEvidence[] }>()
let controller: AbortController | undefined
const error = shallowRef('')
let generation = 0
let alive = true
const labels: Record<TestResult['status'] | 'cancelled' | 'timeout', string> = {
  success: '模型请求成功',
  failed: '检测失败',
  cancelled: '检测已停止',
  timeout: '检测超时',
  unverified: '尚未验证覆写',
}
watch([
  open,
  () => props.account?.account_id,
], ([visible]) => {
  if (visible) {
    result.value = undefined
    error.value = ''
    void loadModels()
  }
  else {
    void stop()
    generation++
    catalogSequence++
  }
})
onBeforeUnmount(() => {
  void stop()
  generation++
  alive = false
  catalogSequence++
})
watch(model, (value) => {
  if (open.value && !modelsLoading.value && value.trim() && props.account)
    remembered.set(props.account.provider_id, value.trim())
})
async function loadModels() {
  const account = props.account
  if (!account)
    return
  const current = ++catalogSequence
  modelsLoading.value = true
  modelsError.value = ''
  models.value = []
  selectedModel.value = undefined
  const last = remembered.get(account.provider_id) ?? ''
  try {
    const value = await getAccountModels(account.account_id)
    if (alive && current === catalogSequence) {
      models.value = value
      selectedModel.value = value.find(item => item.id === last)?.id ?? value[0]?.id
      customModel.value = last
      manualModel.value = !value.length || !!(last && !value.some(item => item.id === last))
    }
  }
  catch (cause) {
    if (alive && current === catalogSequence) {
      modelsError.value = cause instanceof Error
        ? cause.message
        : '模型列表读取失败'
      customModel.value = last
      manualModel.value = true
    }
  }
  finally {
    if (alive && current === catalogSequence)
      modelsLoading.value = false
  }
}
async function run() {
  const accountId = props.account?.account_id
  if (!accountId || active.value || !model.value.trim())
    return
  const current = ++generation
  remembered.set(props.account?.provider_id ?? '', model.value.trim())
  const id = Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, '0')).join('')
  active.value = id
  const started = performance.now()
  const currentController = new AbortController()
  controller = currentController
  let timeout = false
  const timer = window.setTimeout(() => {
    timeout = true
    currentController.abort()
  }, 45000)
  error.value = ''
  result.value = undefined
  try {
    const value = await testAccount(id, accountId, model.value.trim(), currentController.signal)
    if (alive && current === generation)
      result.value = { ...value, elapsedMs: performance.now() - started }
  }
  catch (cause) {
    if (alive && current === generation) {
      if (currentController.signal.aborted) {
        result.value = {
          status: timeout ? 'timeout' : 'cancelled',
          elapsedMs: performance.now() - started,
          outbound: [],
        }
      }
      else {
        error.value = cause instanceof Error
          ? cause.message
          : '测试请求失败'
      }
    }
  }
  finally {
    window.clearTimeout(timer)
    if (controller === currentController)
      controller = undefined
    if (alive && active.value === id) {
      active.value = ''
      stopping.value = false
    }
  }
}
function stop() {
  if (!active.value || stopping.value)
    return
  stopping.value = true
  controller?.abort()
}
</script>

<template>
  <ZDialog v-model="open" title="检测请求头" :description="[account?.name || account?.account_id, account?.email && account.email !== account.name ? account.email : null].filter(Boolean).join(' · ')" width="52rem" :close-on-click-modal="false">
    <form id="account-headers-test" class="space-y-4" @submit.prevent="run">
      <p class="text-z-sm text-z-text-secondary">
        按检测设置定向测试当前账号，最长等待 45 秒
      </p>
      <p class="text-z-xs text-z-text-secondary">
        可在「检测设置」选择已有 API Key，留空时使用共享分组与检测专用 Key
      </p>
      <ZFormItem label="检测模型">
        <div class="space-y-2">
          <ZSelect v-if="!manualModel" v-model="selectedModel" :options="modelOptions" filterable :loading="modelsLoading" :disabled="!!active" placeholder="搜索并选择此账号的模型" aria-label="检测模型" class="w-full" />
          <ZInput v-else v-model="customModel" aria-label="手动模型 ID" placeholder="填写此账号支持的模型 ID" :disabled="!!active" maxlength="256" autocomplete="off" />
          <div class="flex flex-wrap items-center justify-between gap-2">
            <div class="flex items-center gap-2 text-z-sm">
              <ZSwitch v-model="manualModel" :disabled="!!active" aria-label="手动填写模型" />手动填写
            </div>
            <ZButton variant="link" size="small" :loading="modelsLoading" :disabled="!!active" @click="loadModels">
              重新读取模型
            </ZButton>
          </div>
          <p v-if="modelsError" role="alert" class="text-z-sm text-z-error">
            {{ modelsError }}
          </p>
          <p v-else-if="!modelsLoading && !models.length" class="text-z-sm text-z-text-secondary">
            此账号暂无模型目录，可手动填写
          </p>
        </div>
      </ZFormItem>
      <p v-if="active" role="status" class="text-z-sm text-z-text-secondary">
        {{ stopping ? '正在停止检测' : '正在请求上游' }}
      </p>
      <p v-if="error" role="alert" class="rounded-z bg-z-error-container p-3 text-z-error-on-container">
        {{ error }}
      </p>
      <div v-if="result" class="space-y-4" role="status">
        <div class="flex flex-wrap items-center gap-3">
          <ZTag :type="result.status === 'success' ? 'success' : 'info'">
            {{ labels[result.status] }}
          </ZTag>
          <span class="text-z-sm text-z-text-secondary">{{ (result.elapsedMs / 1000).toFixed(2) }} 秒</span>
        </div>
        <p v-if="'message' in result && result.message" class="whitespace-pre-wrap break-words text-z-sm">
          {{ result.message }}
        </p>
        <div v-for="(outbound, index) in result.outbound" :key="index" class="space-y-3 rounded-z bg-z-fill-alter p-4">
          <div class="flex flex-wrap items-center justify-between gap-2 text-z-sm">
            <span>实际覆写后的请求头</span>
            <ZTag size="small" type="info">
              {{ outbound.transport }}
            </ZTag>
          </div>
          <div class="space-y-1 text-z-xs text-z-text-secondary">
            <p :title="outbound.accountId">
              实际账号 {{ account?.name || account?.email || outbound.accountId }}
            </p>
            <p v-if="account?.email && account.email !== account.name" class="break-all">
              {{ account.email }}
            </p>
          </div>
          <p class="text-z-xs text-z-text-secondary">
            第 {{ outbound.attemptIndex }} 次准备 · {{ outbound.overrideEnabled ? '覆写已开启' : '覆写已关闭' }}
          </p>
          <dl class="space-y-3 text-z-sm">
            <div v-for="name in Array.from(new Set(['user-agent', 'originator', ...outbound.headers.map(header => header.name.toLowerCase())]))" :key="name" class="space-y-1">
              <dt class="text-z-text-secondary">
                {{ name }}
              </dt>
              <dd class="m-0 whitespace-pre-wrap break-all font-mono">
                {{ outbound.headers.filter(header => header.name.toLowerCase() === name).map(header => header.value === '' ? '（空值）' : header.value).join('\n') || '（未设置）' }}
              </dd>
            </div>
          </dl>
          <details class="text-z-sm">
            <summary class="cursor-pointer">
              覆写前的身份头
            </summary>
            <dl class="mt-2 space-y-2">
              <div v-for="header in outbound.beforeHeaders" :key="header.name">
                <dt class="text-z-text-secondary">
                  {{ header.name }}
                </dt>
                <dd class="m-0 break-all font-mono">
                  {{ header.value || '（空值）' }}
                </dd>
              </div>
            </dl>
          </details>
        </div>
        <pre v-if="'text' in result && result.text" class="whitespace-pre-wrap break-words text-z-sm">{{ result.text }}</pre>
        <details v-if="'error' in result && result.error" class="text-z-sm">
          <summary class="cursor-pointer">
            模型接口响应（HTTP {{ result.error.upstreamStatus }}）
          </summary>
          <pre class="mt-2 whitespace-pre-wrap break-all">{{ result.error.upstreamBody }}</pre>
        </details>
      </div>
    </form>
    <template #footer>
      <ZButton @click="open = false">
        关闭
      </ZButton>
      <ZButton v-if="active" :loading="stopping" @click="stop">
        停止检测
      </ZButton>
      <ZButton v-else type="primary" native-type="submit" form="account-headers-test" :disabled="!model.trim()">
        开始检测
      </ZButton>
    </template>
  </ZDialog>
</template>
