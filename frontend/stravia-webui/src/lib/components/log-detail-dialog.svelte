<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import DownloadIcon from '@lucide/svelte/icons/download'

import { admin } from '$lib/admin-client'
import { formatDuration, formatLogTime, formatTokenCount, formatTps, computeTps, tryPrettyJson } from '$lib/format'
import type { RequestLog } from '$lib/types'
import { Badge } from '$lib/components/ui/badge'
import { Button, buttonVariants } from '$lib/components/ui/button'
import * as Sheet from '$lib/components/ui/sheet'
import { Skeleton } from '$lib/components/ui/skeleton'
import StatusIndicator from '$lib/components/status-indicator.svelte'

interface Props {
  open?: boolean
  logId?: string
  summary?: RequestLog
}

let { open = $bindable(false), logId, summary }: Props = $props()
const detailQuery = createQuery(() => ({
  queryKey: ['log-detail', logId],
  queryFn: () => admin.logs.get(logId!),
  enabled: open && Boolean(logId),
}))
const log = $derived(detailQuery.data ?? summary)
const tps = $derived(computeTps(log))
const stream = $derived(log?.is_stream ?? (log?.stream_chunks_count ?? 0) > 0)
const crossProtocol = $derived(
  Boolean(log?.client_protocol && log?.upstream_protocol && log.client_protocol !== log.upstream_protocol),
)

function downloadLog(): void {
  if (!log) return
  const empty = m.common_empty()
  const parts = [
    m.log_detail_dialog_download_summary({
      id: log.id,
      time: formatLogTime(log.created_at),
      method: log.method ?? '–',
      path: log.path ?? '–',
      client_status: String(log.client_status_code ?? '–'),
      upstream_status: String(log.upstream_status_code ?? '–'),
      provider: log.provider_name ?? log.provider_id ?? '–',
      model: log.model_name ?? log.model_id ?? '–',
      input_tokens: String(log.input_tokens),
      output_tokens: String(log.output_tokens),
    }),
    '',
    `## ${m.log_detail_dialog_1_client_request_headers()}`,
    log.client_request_headers ?? empty,
    '',
    `## ${m.log_detail_dialog_client_request_body()}`,
    log.client_request_body ?? empty,
    '',
    `## ${m.log_detail_dialog_2_upstream_request_headers()}`,
    log.upstream_request_headers ?? empty,
    '',
    `## ${m.log_detail_dialog_upstream_request_body()}`,
    log.upstream_request_body ?? empty,
    '',
    `## ${m.log_detail_dialog_3_upstream_response_headers()}`,
    log.upstream_response_headers ?? empty,
    '',
    `## ${m.log_detail_dialog_upstream_response_body()}`,
    log.upstream_response_body ?? empty,
    '',
    `## ${m.log_detail_dialog_4_client_response_headers()}`,
    log.client_response_headers ?? empty,
    '',
    `## ${m.log_detail_dialog_client_response_body()}`,
    log.client_response_body ?? empty,
  ]
  const url = URL.createObjectURL(new Blob([parts.join('\n')], { type: 'text/plain' }))
  const anchor = document.createElement('a')
  anchor.href = url
  anchor.download = `stravia-log-${log.id}.log`
  anchor.click()
  URL.revokeObjectURL(url)
}
</script>

<Sheet.Root bind:open>
  <Sheet.Content
    side="right"
    class="route-overlay-content-md w-full! gap-0 overflow-hidden p-0"
    closeLabel={m.log_detail_dialog_close_request_detail()}>
    <Sheet.Header class="border-b"
      ><Sheet.Title class="flex items-center gap-2"
        >{m.log_detail_dialog_request_detail()}{#if detailQuery.isFetching}<span
            class="font-technical text-xs font-normal text-muted-foreground">{m.log_detail_dialog_updating()}</span
          >{/if}</Sheet.Title
      ><Sheet.Description>{log ? formatLogTime(log.created_at) : ''}</Sheet.Description></Sheet.Header>
    {#if log}
      {@const payloads = [
        { title: m.log_detail_dialog_1_client_request_headers(), value: log.client_request_headers },
        { title: m.log_detail_dialog_client_request_body(), value: log.client_request_body },
        { title: m.log_detail_dialog_2_upstream_request_headers(), value: log.upstream_request_headers },
        { title: m.log_detail_dialog_upstream_request_body(), value: log.upstream_request_body },
        { title: m.log_detail_dialog_3_upstream_response_headers(), value: log.upstream_response_headers },
        { title: m.log_detail_dialog_upstream_response_body(), value: log.upstream_response_body },
        { title: m.log_detail_dialog_4_client_response_headers(), value: log.client_response_headers },
        { title: m.log_detail_dialog_client_response_body(), value: log.client_response_body },
      ]}
      <div class="route-overlay-body">
        <div class="flex flex-wrap items-center gap-2 border-b pb-3 text-xs">
          <Badge variant="outline" class="font-mono">{log.method ?? '–'}</Badge><span
            class="break-all font-mono text-muted-foreground">{log.path ?? '–'}</span
          ><StatusIndicator
            compact
            label={String(log.client_status_code ?? '–')}
            tone={log.client_status_code == null ? 'neutral' : log.client_status_code >= 400 ? 'error' : 'healthy'} />
          <Badge variant="outline">{stream ? m.common_sse() : m.common_json()}</Badge>{#if crossProtocol}<Badge
              variant="outline">{m.log_detail_dialog_cross_protocol()}</Badge
            >{/if}{#if log.provider_name ?? log.provider_id}<Badge variant="outline"
              >{log.provider_name ?? log.provider_id}</Badge
            >{/if}<span class="font-technical tabular-nums"
            >{formatDuration(log.latency_total_ms)} · {tps == null ? '–' : formatTps(tps)}</span
          ><span class="font-technical inline-flex gap-2 tabular-nums"
            ><span>{m.common_input_abbreviation()} {formatTokenCount(log.input_tokens)}</span><span
              >{m.common_output_abbreviation()} {formatTokenCount(log.output_tokens)}</span
            ></span>
        </div>
        <div class="mt-4 flex flex-col gap-4">
          {#each payloads as payload (payload.title)}<section>
              <h3 class="font-structural mb-1 text-xs font-semibold tracking-wide text-muted-foreground uppercase">
                {payload.title}
              </h3>
              <pre class="route-code-plane max-h-52 break-all">{payload.value
                  ? tryPrettyJson(payload.value)
                  : m.common_empty()}</pre>
            </section>{/each}
        </div>
      </div>
      <Sheet.Footer class="route-overlay-footer">
        <Sheet.Close class={buttonVariants({ variant: 'outline' })}>{m.log_detail_dialog_close()}</Sheet.Close>
        <Button onclick={downloadLog}><DownloadIcon data-icon="inline-start" />{m.log_detail_dialog_download()}</Button>
      </Sheet.Footer>
    {:else if detailQuery.isPending}
      <div class="route-overlay-body flex flex-col gap-4">
        <Skeleton class="h-12" /><Skeleton class="h-52" /><Skeleton class="h-52" />
      </div>
    {/if}
  </Sheet.Content>
</Sheet.Root>
