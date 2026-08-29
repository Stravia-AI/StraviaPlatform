<script lang="ts">
import { cn } from '$lib/utils'

type Tone = 'healthy' | 'warning' | 'error' | 'neutral'

interface Props {
  label: string
  tone?: Tone
  compact?: boolean
  class?: string
}

let { label, tone = 'neutral', compact = false, class: className }: Props = $props()
</script>

<span class={cn('route-status', compact && 'route-status--compact', className)} data-tone={tone} role="status">
  <span class="route-status__mark" aria-hidden="true"></span>
  <span>{label}</span>
</span>
