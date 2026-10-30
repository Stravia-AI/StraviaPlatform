<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import type { CredentialMatch } from '#lib/types.js'

import { Button } from '#lib/components/ui/button/index.js'
import { cn } from '#lib/utils.js'

interface Props {
  match: CredentialMatch
  ruleNames: ReadonlyMap<string, string>
  /** 命中记录侧栏用：高亮映射对应的匹配，弱化其他匹配；密钥匹配测试保持原样。 */
  emphasis?: 'hit' | 'match'
  onselect: (match: CredentialMatch) => void
}

let { match, ruleNames, emphasis, onselect }: Props = $props()
</script>

<li>
  <Button
    variant="outline"
    class={cn(
      'h-auto w-full justify-start whitespace-normal py-3 text-left',
      emphasis === 'hit' && 'border-primary/60',
      emphasis === 'match' && 'opacity-60',
    )}
    data-emphasis={emphasis}
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
