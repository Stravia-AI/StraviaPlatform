<script lang="ts">
import { Tabs as TabsPrimitive } from 'bits-ui'

import { cn } from '$lib/utils.js'
import { tabsTriggerVariants } from './variants.js'

let { ref = $bindable(null), class: className, ...restProps }: TabsPrimitive.TriggerProps = $props()
</script>

<TabsPrimitive.Trigger bind:ref data-slot="tabs-trigger" class={cn(tabsTriggerVariants(), className)} {...restProps} />
