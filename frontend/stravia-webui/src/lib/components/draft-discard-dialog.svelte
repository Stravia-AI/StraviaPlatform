<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import type { DraftGuard } from '#lib/draft-guard.svelte.js'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { buttonVariants } from '#lib/components/ui/button/index.js'

interface Props {
  guard: DraftGuard
  title?: string
  description?: string
}

let { guard, title = m.draft_guard_title(), description = m.draft_guard_description() }: Props = $props()
</script>

<AlertDialog.Root
  open={guard.confirming}
  onOpenChange={(open: boolean) => {
    if (!open) guard.keepEditing()
  }}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{title}</AlertDialog.Title>
      <AlertDialog.Description>{description}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.draft_guard_keep_editing()}</AlertDialog.Cancel>
      <AlertDialog.Action class={buttonVariants({ variant: 'destructive' })} onclick={() => void guard.confirmDiscard()}
        >{m.draft_guard_discard()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
