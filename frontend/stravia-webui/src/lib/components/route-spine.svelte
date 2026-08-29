<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'

interface Props {
  apiKeyCount?: number
  enabledModelCount?: number
  providerCount?: number
  currentPath?: string
}

let { apiKeyCount, enabledModelCount, providerCount, currentPath = '/' }: Props = $props()

const stages = $derived([
  {
    key: 'ingress',
    href: '/api-keys' as const,
    label: m.route_spine_client_access(),
    value:
      apiKeyCount == null ? m.common_unavailable() : m.route_spine_value_api_keys({ apiKeyCount: apiKeyCount }),
    detail: apiKeyCount === 0 ? m.common_create_api_key() : m.route_spine_address_used_apps(),
  },
  {
    key: 'models',
    href: '/models' as const,
    label: m.common_models(),
    value:
      enabledModelCount == null
        ? m.common_unavailable()
        : m.route_spine_value_available({ enabledModelCount: enabledModelCount }),
    detail: enabledModelCount === 0 ? m.route_spine_add_model() : m.route_spine_names_apps_request(),
  },
  {
    key: 'providers',
    href: '/providers' as const,
    label: m.common_model_services(),
    value:
      providerCount == null ? m.common_unavailable() : m.route_spine_value_connected({ providerCount: providerCount }),
    detail: providerCount === 0 ? m.common_connect_model_service() : m.route_spine_where_model_requests_sent(),
  },
])
</script>

<nav class="route-spine" aria-label={m.route_spine_request_path()}>
  <div class="route-spine__heading">
    <p class="font-structural text-[0.7rem] font-semibold tracking-[0.14em] text-primary uppercase">
      {m.route_spine_request_path()}
    </p>
    <p class="text-xs text-muted-foreground">
      {m.route_spine_app_selected_ai_service()}
    </p>
  </div>
  <ol class="route-spine__stages">
    {#each stages as stage, index (stage.key)}
      <li
        class="route-spine__stage"
        data-active={currentPath === stage.href || currentPath.startsWith(`${stage.href}/`)}>
        <a href={resolve(stage.href)} aria-current={currentPath === stage.href ? 'location' : undefined}>
          <span class="route-spine__index" aria-hidden="true">{String(index + 1).padStart(2, '0')}</span>
          <span class="min-w-0">
            <span class="route-spine__label">{stage.label}</span>
            <span class="route-spine__value">{stage.value}</span>
            <span class="route-spine__detail">{stage.detail}</span>
          </span>
        </a>
      </li>
    {/each}
  </ol>
</nav>
