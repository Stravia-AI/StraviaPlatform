<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import type { CredentialMatch } from '#lib/types.js'

import { Button } from '#lib/components/ui/button/index.js'

interface Props {
  match: CredentialMatch
  ruleNames: ReadonlyMap<string, string>
  onselect: (match: CredentialMatch) => void
}

let { match, ruleNames, onselect }: Props = $props()
</script>

<li>
  <Button
    variant="outline"
    class="h-auto w-full justify-start whitespace-normal py-3 text-left"
    onclick={() => onselect(match)}>
    <span class="flex min-w-0 flex-col gap-1">
      <span class="break-all"
        >{ruleNames.get(match.rule_id) ?? match.rule_id}
        <span class="text-xs text-muted-foreground">({match.rule_id})</span></span>
      <span
        >{m.credential_protection_tester_position({
          startLine: match.start_line,
          startColumn: match.start_column,
          endLine: match.end_line,
          endColumn: match.end_column,
        })}</span>
    </span>
  </Button>
</li>
