<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { admin as api } from '#lib/admin-client.js'
import { localizeMarketplaceError } from '#lib/backend-error.js'
import { openExternalUrl } from '#lib/open-external.js'
import type { MarketplaceError, PluginSourceSummary, SourcePreview } from '#lib/types.js'
import TechnicalValue from '#lib/components/technical-value.svelte'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Label } from '#lib/components/ui/label/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'

let { open = $bindable(false), onchanged }: { open?: boolean; onchanged: () => void } = $props()
const queryClient = useQueryClient()
const sourcesQuery = createQuery(() => ({
  queryKey: ['plugin-sources'],
  queryFn: api.pluginSources.list,
  enabled: open,
}))
let adding = $state(false)
let url = $state('')
let fingerprint = $state('')
let preview = $state<SourcePreview>()
let trustSource = $state<PluginSourceSummary>()
let deleteSource = $state<PluginSourceSummary>()
let busy = $state(false)
let error = $state<unknown>()

function errorCode(value: unknown): string | undefined {
  if (value && typeof value === 'object' && 'code' in value && typeof value.code === 'string') return value.code
  return undefined
}

function errorMessage(value: unknown): string {
  return localizeMarketplaceError(value)
}

function releaseUrl(value: string): string | undefined {
  try {
    const parsed = new URL(value)
    if (
      parsed.protocol !== 'https:' ||
      parsed.hostname !== 'github.com' ||
      parsed.port ||
      parsed.username ||
      parsed.password ||
      parsed.search ||
      parsed.hash
    )
      return undefined
    const parts = parsed.pathname.split('/').filter(Boolean)
    if (parts.length !== 2) return undefined
    return `https://github.com/${parts[0]}/${parts[1].replace(/\.git$/, '')}/releases`
  } catch {
    return undefined
  }
}

function resetFlow() {
  adding = false
  preview = undefined
  trustSource = undefined
  deleteSource = undefined
  error = undefined
}

async function changed() {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ['plugin-sources'] }),
    queryClient.invalidateQueries({ queryKey: ['plugin-marketplace'] }),
  ])
  onchanged()
}

async function run(action: () => Promise<void>) {
  busy = true
  error = undefined
  try {
    await action()
  } catch (caught) {
    error = caught
  } finally {
    busy = false
  }
}

function addPreview() {
  void run(async () => {
    preview = await api.pluginSources.preview({ url: url.trim(), fingerprint: fingerprint.trim() || undefined })
  })
}
function reviewKey(source: PluginSourceSummary) {
  trustSource = source
  void run(async () => {
    preview = await api.pluginSources.trustPreview(source.id)
  })
}
function confirmTrust() {
  if (!preview) return
  const current = preview
  void run(async () => {
    if (trustSource) await api.pluginSources.trustConfirm(trustSource.id, { preview_id: current.preview_id })
    else await api.pluginSources.confirm({ preview_id: current.preview_id })
    resetFlow()
    url = ''
    fingerprint = ''
    await changed()
  })
}
function update(source: PluginSourceSummary, patch: { enabled?: boolean; include_prerelease?: boolean }) {
  void run(async () => {
    await api.pluginSources.update(source.id, patch)
    await changed()
  })
}
function refresh(source: PluginSourceSummary) {
  void run(async () => {
    await api.pluginMarketplace.refresh({ source_id: source.id })
    await changed()
  })
}
function remove(source: PluginSourceSummary) {
  void run(async () => {
    await api.pluginSources.delete(source.id)
    resetFlow()
    await changed()
  })
}
function sourceError(value: MarketplaceError): string {
  return errorMessage(value)
}
function dateLabel(timestamp: number): string {
  return new Date(timestamp < 100_000_000_000 ? timestamp * 1000 : timestamp).toLocaleString()
}
</script>

<Sheet.Root bind:open>
  <Sheet.Content side="right" class="w-full! gap-0 sm:max-w-xl!" closeLabel={m.common_close()}>
    <Sheet.Header class="pr-12">
      <Sheet.Title>{m.plugin_sources_title()}</Sheet.Title>
      <Sheet.Description>{m.plugin_sources_description()}</Sheet.Description>
    </Sheet.Header>
    <div class="route-overlay-body space-y-5">
      {#if error}
        <div role="alert" class="rounded-lg border border-destructive/40 bg-destructive/5 p-3 text-sm text-destructive">
          {errorMessage(error)}
          {#if errorCode(error) === 'not_cataloged' && releaseUrl(url)}
            <button type="button" onclick={() => void openExternalUrl(releaseUrl(url)!)} class="ml-1 underline"
              >{m.plugin_sources_releases()}</button>
          {/if}
        </div>
      {/if}
      {#if preview}
        <div class="space-y-4">
          <div>
            <p class="text-xs text-muted-foreground">{m.plugin_sources_repository()}</p>
            <button
              type="button"
              onclick={() => void openExternalUrl(preview!.url)}
              class="break-all font-medium underline">{preview.repository}</button>
          </div>
          {#if trustSource}
            <div>
              <p class="mb-1 text-xs text-muted-foreground">{m.plugin_sources_old_fingerprint()}</p>
              <TechnicalValue
                value={preview.previous_fingerprint ?? trustSource.fingerprint}
                class="whitespace-normal break-all"
                copyable />
            </div>
          {/if}
          <div>
            <p class="mb-1 text-xs text-muted-foreground">
              {trustSource ? m.plugin_sources_new_fingerprint() : m.plugin_sources_fingerprint()}
            </p>
            <TechnicalValue value={preview.fingerprint} class="whitespace-normal break-all" copyable />
          </div>
          <p class="rounded-lg border border-warning/40 bg-warning/5 p-3 text-sm">{m.plugin_sources_risk()}</p>
          <div>
            <p class="mb-2 font-medium">{m.plugin_sources_plugins()}</p>
            {#if preview.releases.length}
              <ul class="space-y-1 text-sm">
                {#each preview.releases as release (JSON.stringify([release.vendor_id, release.version]))}
                  <li>
                    {release.display_name}
                    <span class="text-muted-foreground">({release.vendor_id} · {release.version})</span>
                  </li>
                {/each}
              </ul>
            {:else}<p class="text-sm text-muted-foreground">{m.plugin_sources_no_plugins()}</p>{/if}
          </div>
          <div class="flex flex-wrap gap-2">
            <Button
              variant="outline"
              disabled={busy}
              onclick={() => {
                preview = undefined
                trustSource = undefined
                error = undefined
              }}>{m.plugin_sources_back()}</Button>
            <Button disabled={busy} onclick={confirmTrust}
              >{#if busy}<Spinner />{/if}{trustSource
                ? m.plugin_sources_confirm_retrust()
                : m.plugin_sources_trust()}</Button>
          </div>
        </div>
      {:else if adding}
        <form
          class="space-y-4"
          onsubmit={(event) => {
            event.preventDefault()
            addPreview()
          }}>
          <div class="space-y-2">
            <Label for="plugin-source-url">{m.plugin_sources_url()}</Label><Input
              id="plugin-source-url"
              type="url"
              bind:value={url}
              placeholder="https://github.com/owner/repo"
              required />
          </div>
          <div class="space-y-2">
            <Label for="plugin-source-fingerprint">{m.plugin_sources_fingerprint_optional()}</Label><Input
              id="plugin-source-fingerprint"
              bind:value={fingerprint}
              autocomplete="off" />
          </div>
          <div class="flex flex-wrap gap-2">
            <Button type="button" variant="outline" disabled={busy} onclick={resetFlow}
              >{m.plugin_sources_cancel()}</Button
            ><Button type="submit" disabled={busy}
              >{#if busy}<Spinner />{/if}{m.plugin_sources_preview()}</Button>
          </div>
        </form>
      {:else}
        <Button
          onclick={() => {
            adding = true
            error = undefined
          }}>{m.plugin_sources_add()}</Button>
        {#if sourcesQuery.isPending}
          <p class="flex items-center gap-2 text-sm text-muted-foreground"><Spinner />{m.plugin_sources_loading()}</p>
        {:else if sourcesQuery.error}
          <div role="alert" class="space-y-2 text-sm">
            <p>{errorMessage(sourcesQuery.error)}</p>
            <Button variant="outline" onclick={() => void sourcesQuery.refetch()}>{m.plugin_sources_retry()}</Button>
          </div>
        {:else if !sourcesQuery.data?.length}
          <p class="text-sm text-muted-foreground">{m.plugin_sources_empty()}</p>
        {:else}
          <div class="divide-y rounded-lg border">
            {#each sourcesQuery.data as source (source.id)}
              <section class="space-y-3 p-4">
                <div class="flex flex-wrap items-center gap-2">
                  <button
                    type="button"
                    onclick={() => void openExternalUrl(source.url)}
                    class="min-w-0 break-all font-medium underline">{source.repository}</button
                  >{#if source.official}<Badge variant="outline">{m.plugin_sources_official()}</Badge>{/if}
                </div>
                <p class="text-xs text-muted-foreground">
                  {source.mirrored === true
                    ? m.plugin_sources_mirrored()
                    : source.mirrored === false
                      ? m.plugin_sources_not_mirrored()
                      : m.plugin_sources_mirror_unknown()}
                </p>
                <p class="text-xs text-muted-foreground">
                  {source.succeeded_at != null
                    ? m.plugin_sources_last_success({ date: dateLabel(source.succeeded_at) })
                    : m.plugin_sources_never_checked()}
                </p>
                <div class="flex flex-wrap gap-x-6 gap-y-3 text-sm">
                  <label class="flex items-center gap-2"
                    ><Switch
                      bind:checked={() => source.enabled, (checked: boolean) => update(source, { enabled: checked })}
                      disabled={busy} />{m.plugin_sources_enabled()}</label>
                  <label class="flex items-center gap-2"
                    ><Switch
                      bind:checked={
                        () => source.include_prerelease,
                        (checked: boolean) => update(source, { include_prerelease: checked })
                      }
                      disabled={busy} />{m.plugin_sources_prerelease()}</label>
                </div>
                {#if source.error}<p role="alert" class="text-sm text-destructive">{sourceError(source.error)}</p>{/if}
                <div class="flex flex-wrap gap-2">
                  <Button variant="outline" size="sm" disabled={busy || !source.enabled} onclick={() => refresh(source)}
                    >{m.plugin_sources_refresh()}</Button>
                  {#if source.error?.code === 'key_changed' && !source.official}<Button
                      variant="outline"
                      size="sm"
                      disabled={busy}
                      onclick={() => reviewKey(source)}>{m.plugin_sources_retrust()}</Button
                    >{/if}
                  {#if !source.official}<Button
                      variant="outline"
                      size="sm"
                      disabled={busy}
                      onclick={() => {
                        deleteSource = source
                        error = undefined
                      }}>{m.plugin_sources_delete()}</Button
                    >{/if}
                </div>
              </section>
            {/each}
          </div>
        {/if}
      {/if}
    </div>
  </Sheet.Content>
</Sheet.Root>

<AlertDialog.Root
  bind:open={
    () => !!deleteSource,
    (next) => {
      if (!next && !busy) {
        deleteSource = undefined
        error = undefined
      }
    }
  }>
  {#if deleteSource}
    <AlertDialog.Content class="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-lg">
      <AlertDialog.Header>
        <AlertDialog.Title>{m.plugin_sources_delete()}</AlertDialog.Title>
        <AlertDialog.Description
          >{m.plugin_sources_delete_prompt({ repository: deleteSource.repository })}</AlertDialog.Description>
      </AlertDialog.Header>
      {#if error}<p role="alert" class="text-sm text-destructive">{errorMessage(error)}</p>{/if}
      <AlertDialog.Footer>
        <AlertDialog.Cancel disabled={busy}>{m.plugin_sources_cancel()}</AlertDialog.Cancel>
        <AlertDialog.Action
          variant="destructive"
          disabled={busy}
          aria-busy={busy}
          onclick={(event: MouseEvent) => {
            event.preventDefault()
            if (deleteSource) remove(deleteSource)
          }}>
          {#if busy}<Spinner />{/if}{m.plugin_sources_delete()}
        </AlertDialog.Action>
      </AlertDialog.Footer>
    </AlertDialog.Content>
  {/if}
</AlertDialog.Root>
