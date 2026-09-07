<script lang="ts">
import { Pagination as PaginationPrimitive } from 'bits-ui'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import { buttonVariants } from '$lib/components/ui/button/index.js'
import { cn } from '$lib/utils.js'

let { ref = $bindable(null), class: className, children, ...restProps }: PaginationPrimitive.NextButtonProps = $props()
</script>

<PaginationPrimitive.NextButton
  bind:ref
  aria-label="Go to next page"
  class={cn(buttonVariants({ variant: 'ghost', size: 'default' }), 'pr-1.5!', className)}
  {...restProps}>
  {#if children}
    {@render children()}
  {:else}
    <span class="cn-pagination-next-text hidden sm:block">Next</span>
    <ChevronRightIcon data-icon="inline-end" />
  {/if}
</PaginationPrimitive.NextButton>
