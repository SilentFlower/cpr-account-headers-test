<script setup lang="ts">
import type { Account, RuleSnapshot } from '../api/modules/headers'
import { ZButton, ZDialog, ZEmpty, ZTag } from '@codex-proxy/ui'
import { onBeforeUnmount, shallowRef, watch } from 'vue'
import { getRule } from '../api/modules/headers'

const props = defineProps<{ account?: Account }>()
const open = defineModel<boolean>({ required: true })
const rule = shallowRef<RuleSnapshot>()
const loading = shallowRef(false)
const error = shallowRef('')
let sequence = 0
let alive = true
watch([
  open,
  () => props.account?.account_id,
], ([visible]) => {
  if (visible)
    void load()
  else
    sequence++
})
onBeforeUnmount(() => {
  alive = false
  sequence++
})
async function load() {
  const accountId = props.account?.account_id
  if (!accountId)
    return
  const current = ++sequence
  loading.value = true
  rule.value = undefined
  error.value = ''
  try {
    const value = await getRule(accountId)
    if (alive && current === sequence)
      rule.value = value
  }
  catch (cause) {
    if (alive && current === sequence) {
      error.value = cause instanceof Error
        ? cause.message
        : '请求头规则读取失败'
    }
  }
  finally {
    if (alive && current === sequence)
      loading.value = false
  }
}
</script>

<template>
  <ZDialog v-model="open" title="查看请求头" :description="[account?.name || account?.account_id, account?.email && account.email !== account.name ? account.email : null].filter(Boolean).join(' · ')" width="44rem">
    <div class="space-y-4">
      <p class="text-z-sm text-z-text-secondary">
        此处只读展示，编辑规则请前往「账号请求头」插件
      </p>
      <p v-if="loading" role="status" class="text-z-sm text-z-text-secondary">
        正在读取规则
      </p>
      <div v-if="error" role="alert" class="space-y-2 rounded-z bg-z-error-container p-3 text-z-error-on-container">
        <p>{{ error }}</p>
        <ZButton size="small" @click="load">
          重新读取
        </ZButton>
      </div>
      <template v-if="rule">
        <ZTag :type="rule.enabled ? 'success' : 'info'">
          {{ rule.enabled ? '覆写已开启' : '覆写已关闭' }}
        </ZTag>
        <dl v-if="rule.headers.length" class="space-y-3 rounded-z bg-z-fill-alter p-4 text-z-sm">
          <div v-for="header in rule.headers" :key="header.name" class="space-y-1">
            <dt class="font-emphasis">
              {{ header.name }}
            </dt>
            <dd class="m-0 whitespace-pre-wrap break-all font-mono text-z-text-secondary">
              {{ header.value || '（空值）' }}
            </dd>
          </div>
        </dl>
        <ZEmpty v-else title="此账号尚未配置请求头" surface="none" />
      </template>
    </div>
    <template #footer>
      <ZButton @click="open = false">
        关闭
      </ZButton>
    </template>
  </ZDialog>
</template>
