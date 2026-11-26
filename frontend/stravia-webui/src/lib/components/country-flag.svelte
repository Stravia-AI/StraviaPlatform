<script lang="ts" module>
const KNOWN_FLAGS = new Set([
  'us',
  'jp',
  'hk',
  'sg',
  'tw',
  'gb',
  'de',
  'fr',
  'ca',
  'au',
  'kr',
  'nl',
  'ch',
  'se',
  'no',
  'fi',
  'ie',
  'it',
  'es',
  'br',
  'in',
  'cn',
  'ru',
  'ua',
  'pl',
  'tr',
  'ae',
  'il',
  'za',
  'mx',
  'nz',
  'be',
  'at',
  'cz',
  'dk',
  'pt',
  'my',
  'th',
  'vn',
  'id',
  'ph',
  'ro',
  'bg',
  'hu',
  'gr',
  'lu',
  'is',
  'ar',
  'cl',
  'co',
])
</script>

<script lang="ts">
import GlobeIcon from '@lucide/svelte/icons/globe'

import { cn } from '#lib/utils.js'

interface Props {
  code: string | null | undefined
  class?: string
}

let { code, class: className }: Props = $props()

const normalized = $derived.by(() => {
  const value = code?.trim().toLowerCase() ?? ''
  return /^[a-z]{2}$/.test(value) ? value : null
})
const hasSvgFlag = $derived(normalized !== null && KNOWN_FLAGS.has(normalized))
const upperCode = $derived(normalized ? normalized.toUpperCase() : '')
</script>

{#if hasSvgFlag && normalized}
  <span
    role="img"
    aria-label={upperCode}
    class={cn(
      'relative inline-flex h-3 w-4 shrink-0 overflow-hidden rounded-[2px] align-middle',
      'after:pointer-events-none after:absolute after:inset-0 after:rounded-[2px] after:ring-1 after:ring-inset after:ring-black/10 dark:after:ring-white/15',
      className,
    )}>
    <svg viewBox="0 0 24 18" class="size-full" aria-hidden="true">
      {#if normalized === 'us'}
        <rect width="24" height="18" fill="#fff" />
        <g fill="#b22234">
          <rect y="0" width="24" height="1.385" />
          <rect y="2.77" width="24" height="1.385" />
          <rect y="5.54" width="24" height="1.385" />
          <rect y="8.31" width="24" height="1.385" />
          <rect y="11.08" width="24" height="1.385" />
          <rect y="13.85" width="24" height="1.385" />
          <rect y="16.615" width="24" height="1.385" />
        </g>
        <rect width="10.4" height="9.69" fill="#3c3b6e" />
        <g fill="#fff">
          <circle cx="1.8" cy="1.8" r="0.55" />
          <circle cx="4.0" cy="1.8" r="0.55" />
          <circle cx="6.2" cy="1.8" r="0.55" />
          <circle cx="8.4" cy="1.8" r="0.55" />
          <circle cx="2.9" cy="3.6" r="0.55" />
          <circle cx="5.1" cy="3.6" r="0.55" />
          <circle cx="7.3" cy="3.6" r="0.55" />
          <circle cx="1.8" cy="5.4" r="0.55" />
          <circle cx="4.0" cy="5.4" r="0.55" />
          <circle cx="6.2" cy="5.4" r="0.55" />
          <circle cx="8.4" cy="5.4" r="0.55" />
          <circle cx="2.9" cy="7.2" r="0.55" />
          <circle cx="5.1" cy="7.2" r="0.55" />
          <circle cx="7.3" cy="7.2" r="0.55" />
        </g>
      {:else if normalized === 'jp'}
        <rect width="24" height="18" fill="#fff" />
        <circle cx="12" cy="9" r="5" fill="#bc002d" />
      {:else if normalized === 'hk'}
        <rect width="24" height="18" fill="#de2910" />
        <g fill="#fff" transform="translate(12 9)">
          <path d="M0-5.4C1.8-4.5 2.2-2.2 0.6-0.7C-0.4-1.6-0.9-3.6 0-5.4Z" />
          <path d="M0-5.4C1.8-4.5 2.2-2.2 0.6-0.7C-0.4-1.6-0.9-3.6 0-5.4Z" transform="rotate(72)" />
          <path d="M0-5.4C1.8-4.5 2.2-2.2 0.6-0.7C-0.4-1.6-0.9-3.6 0-5.4Z" transform="rotate(144)" />
          <path d="M0-5.4C1.8-4.5 2.2-2.2 0.6-0.7C-0.4-1.6-0.9-3.6 0-5.4Z" transform="rotate(216)" />
          <path d="M0-5.4C1.8-4.5 2.2-2.2 0.6-0.7C-0.4-1.6-0.9-3.6 0-5.4Z" transform="rotate(288)" />
        </g>
      {:else if normalized === 'sg'}
        <rect width="24" height="9" fill="#ef3340" />
        <rect y="9" width="24" height="9" fill="#fff" />
        <circle cx="6.1" cy="4.5" r="3" fill="#fff" />
        <circle cx="7.2" cy="4.5" r="2.55" fill="#ef3340" />
        <g fill="#fff">
          <circle cx="8.6" cy="2.9" r="0.55" />
          <circle cx="9.9" cy="4.0" r="0.55" />
          <circle cx="9.4" cy="5.6" r="0.55" />
          <circle cx="7.8" cy="5.6" r="0.55" />
          <circle cx="7.3" cy="4.0" r="0.55" />
        </g>
      {:else if normalized === 'tw'}
        <rect width="24" height="18" fill="#fe0000" />
        <rect width="12" height="9" fill="#000095" />
        <g transform="translate(6 4.5)">
          <polygon
            fill="#fff"
            points="0,-3.3 0.7,-1.7 2.3,-2.3 1.7,-0.7 3.3,0 1.7,0.7 2.3,2.3 0.7,1.7 0,3.3 -0.7,1.7 -2.3,2.3 -1.7,0.7 -3.3,0 -1.7,-0.7 -2.3,-2.3 -0.7,-1.7" />
          <circle r="1.95" fill="#000095" />
          <circle r="1.5" fill="#fff" />
        </g>
      {:else if normalized === 'gb'}
        <rect width="24" height="18" fill="#012169" />
        <path d="M0 0L24 18M24 0L0 18" stroke="#fff" stroke-width="3.6" />
        <path d="M0 0L24 18M24 0L0 18" stroke="#c8102e" stroke-width="1.6" />
        <path d="M12 0V18M0 9H24" stroke="#fff" stroke-width="6" />
        <path d="M12 0V18M0 9H24" stroke="#c8102e" stroke-width="3.4" />
      {:else if normalized === 'de'}
        <rect width="24" height="6" fill="#000" />
        <rect y="6" width="24" height="6" fill="#dd0000" />
        <rect y="12" width="24" height="6" fill="#ffce00" />
      {:else if normalized === 'fr'}
        <rect width="8" height="18" fill="#002395" />
        <rect x="8" width="8" height="18" fill="#fff" />
        <rect x="16" width="8" height="18" fill="#ed2939" />
      {:else if normalized === 'ca'}
        <rect width="24" height="18" fill="#fff" />
        <rect width="6" height="18" fill="#d80621" />
        <rect x="18" width="6" height="18" fill="#d80621" />
        <path
          fill="#d80621"
          d="M12 3.4L10.7 6.3L9.3 5.8L9.9 8.6L7.7 7.6L8.2 9.1L6.7 9.8L8.8 11.4L8.4 12.6L11.4 12.2L11.4 14.8H12.6V12.2L15.6 12.6L15.2 11.4L17.3 9.8L15.8 9.1L16.3 7.6L14.1 8.6L14.7 5.8L13.3 6.3Z" />
      {:else if normalized === 'au'}
        <rect width="24" height="18" fill="#012169" />
        <svg x="0" y="0" width="12" height="9" viewBox="0 0 24 18">
          <rect width="24" height="18" fill="#012169" />
          <path d="M0 0L24 18M24 0L0 18" stroke="#fff" stroke-width="4" />
          <path d="M0 0L24 18M24 0L0 18" stroke="#c8102e" stroke-width="1.8" />
          <path d="M12 0V18M0 9H24" stroke="#fff" stroke-width="6.4" />
          <path d="M12 0V18M0 9H24" stroke="#c8102e" stroke-width="3.6" />
        </svg>
        <g fill="#fff">
          <circle cx="6" cy="13.5" r="1.45" />
          <circle cx="18" cy="4.2" r="0.9" />
          <circle cx="21" cy="7.5" r="0.85" />
          <circle cx="15.2" cy="8.4" r="0.85" />
          <circle cx="18" cy="14.2" r="1" />
          <circle cx="19.8" cy="10.8" r="0.55" />
        </g>
      {:else if normalized === 'kr'}
        <rect width="24" height="18" fill="#fff" />
        <g transform="translate(12 9) rotate(-32)">
          <path d="M-4.2 0A4.2 4.2 0 0 1 4.2 0Z" fill="#cd2e3a" />
          <path d="M-4.2 0A4.2 4.2 0 0 0 4.2 0Z" fill="#0047a0" />
          <circle cx="-2.1" cy="0" r="2.1" fill="#cd2e3a" />
          <circle cx="2.1" cy="0" r="2.1" fill="#0047a0" />
        </g>
        <g stroke="#111" stroke-width="0.95">
          <g transform="translate(5 4.2) rotate(-56)">
            <line x1="-2.2" y1="-1.3" x2="2.2" y2="-1.3" />
            <line x1="-2.2" y1="0" x2="2.2" y2="0" />
            <line x1="-2.2" y1="1.3" x2="2.2" y2="1.3" />
          </g>
          <g transform="translate(19 4.2) rotate(56)">
            <line x1="-2.2" y1="-1.3" x2="-0.3" y2="-1.3" />
            <line x1="0.3" y1="-1.3" x2="2.2" y2="-1.3" />
            <line x1="-2.2" y1="0" x2="2.2" y2="0" />
            <line x1="-2.2" y1="1.3" x2="-0.3" y2="1.3" />
            <line x1="0.3" y1="1.3" x2="2.2" y2="1.3" />
          </g>
          <g transform="translate(5 13.8) rotate(56)">
            <line x1="-2.2" y1="-1.3" x2="2.2" y2="-1.3" />
            <line x1="-2.2" y1="0" x2="-0.3" y2="0" />
            <line x1="0.3" y1="0" x2="2.2" y2="0" />
            <line x1="-2.2" y1="1.3" x2="2.2" y2="1.3" />
          </g>
          <g transform="translate(19 13.8) rotate(-56)">
            <line x1="-2.2" y1="-1.3" x2="-0.3" y2="-1.3" />
            <line x1="0.3" y1="-1.3" x2="2.2" y2="-1.3" />
            <line x1="-2.2" y1="0" x2="-0.3" y2="0" />
            <line x1="0.3" y1="0" x2="2.2" y2="0" />
            <line x1="-2.2" y1="1.3" x2="-0.3" y2="1.3" />
            <line x1="0.3" y1="1.3" x2="2.2" y2="1.3" />
          </g>
        </g>
      {:else if normalized === 'nl'}
        <rect width="24" height="6" fill="#ae1c28" />
        <rect y="6" width="24" height="6" fill="#fff" />
        <rect y="12" width="24" height="6" fill="#21468b" />
      {:else if normalized === 'ch'}
        <rect width="24" height="18" fill="#d52b1e" />
        <rect x="10.2" y="4" width="3.6" height="10" fill="#fff" />
        <rect x="7" y="7.2" width="10" height="3.6" fill="#fff" />
      {:else if normalized === 'se'}
        <rect width="24" height="18" fill="#006aa7" />
        <rect x="7" width="3.2" height="18" fill="#fecc00" />
        <rect y="7.4" width="24" height="3.2" fill="#fecc00" />
      {:else if normalized === 'no'}
        <rect width="24" height="18" fill="#ba0c2f" />
        <rect x="6.4" width="4.4" height="18" fill="#fff" />
        <rect y="6.8" width="24" height="4.4" fill="#fff" />
        <rect x="7.5" width="2.2" height="18" fill="#00205b" />
        <rect y="7.9" width="24" height="2.2" fill="#00205b" />
      {:else if normalized === 'fi'}
        <rect width="24" height="18" fill="#fff" />
        <rect x="6.8" width="4" height="18" fill="#003580" />
        <rect y="7" width="24" height="4" fill="#003580" />
      {:else if normalized === 'ie'}
        <rect width="8" height="18" fill="#169b62" />
        <rect x="8" width="8" height="18" fill="#fff" />
        <rect x="16" width="8" height="18" fill="#ff883e" />
      {:else if normalized === 'it'}
        <rect width="8" height="18" fill="#009246" />
        <rect x="8" width="8" height="18" fill="#fff" />
        <rect x="16" width="8" height="18" fill="#ce2b37" />
      {:else if normalized === 'es'}
        <rect width="24" height="4.5" fill="#aa151b" />
        <rect y="4.5" width="24" height="9" fill="#f1bf00" />
        <rect y="13.5" width="24" height="4.5" fill="#aa151b" />
        <rect x="6.2" y="7" width="2.8" height="4" rx="0.8" fill="#aa151b" />
        <rect x="6.7" y="7.5" width="1.8" height="1.5" fill="#f1bf00" />
      {:else if normalized === 'br'}
        <rect width="24" height="18" fill="#009c3b" />
        <polygon points="12,2.2 21.8,9 12,15.8 2.2,9" fill="#ffdf00" />
        <circle cx="12" cy="9" r="3.8" fill="#002776" />
        <path d="M8.5 8.1A6.5 6.5 0 0 1 15.6 10.2" fill="none" stroke="#fff" stroke-width="1" />
      {:else if normalized === 'in'}
        <rect width="24" height="6" fill="#ff9933" />
        <rect y="6" width="24" height="6" fill="#fff" />
        <rect y="12" width="24" height="6" fill="#138808" />
        <circle cx="12" cy="9" r="2.1" fill="none" stroke="#000080" stroke-width="0.7" />
        <circle cx="12" cy="9" r="0.55" fill="#000080" />
      {:else if normalized === 'cn'}
        <rect width="24" height="18" fill="#de2910" />
        <g fill="#ffde00">
          <polygon points="5,2.5 5.65,4.4 7.6,4.4 6,5.55 6.6,7.4 5,6.25 3.4,7.4 4,5.55 2.4,4.4 4.35,4.4" />
          <circle cx="9.2" cy="2.6" r="0.75" />
          <circle cx="10.6" cy="4.4" r="0.75" />
          <circle cx="10.6" cy="6.6" r="0.75" />
          <circle cx="9.2" cy="8.4" r="0.75" />
        </g>
      {:else if normalized === 'ru'}
        <rect width="24" height="6" fill="#fff" />
        <rect y="6" width="24" height="6" fill="#0039a6" />
        <rect y="12" width="24" height="6" fill="#d52b1e" />
      {:else if normalized === 'ua'}
        <rect width="24" height="9" fill="#0057b7" />
        <rect y="9" width="24" height="9" fill="#ffd700" />
      {:else if normalized === 'pl'}
        <rect width="24" height="9" fill="#fff" />
        <rect y="9" width="24" height="9" fill="#dc143c" />
      {:else if normalized === 'tr'}
        <rect width="24" height="18" fill="#e30a17" />
        <circle cx="9.2" cy="9" r="4.2" fill="#fff" />
        <circle cx="10.5" cy="9" r="3.35" fill="#e30a17" />
        <polygon
          fill="#fff"
          points="14.5,6.9 15.1,8.5 16.8,8.5 15.4,9.5 15.9,11.1 14.5,10.1 13.1,11.1 13.6,9.5 12.2,8.5 13.9,8.5" />
      {:else if normalized === 'ae'}
        <rect x="6" width="18" height="6" fill="#00732f" />
        <rect x="6" y="6" width="18" height="6" fill="#fff" />
        <rect x="6" y="12" width="18" height="6" fill="#000" />
        <rect width="6" height="18" fill="#ff0000" />
      {:else if normalized === 'il'}
        <rect width="24" height="18" fill="#fff" />
        <rect y="2" width="24" height="2.4" fill="#0038b8" />
        <rect y="13.6" width="24" height="2.4" fill="#0038b8" />
        <polygon points="12,5.6 14.9,10.7 9.1,10.7" fill="none" stroke="#0038b8" stroke-width="0.95" />
        <polygon points="12,12.4 14.9,7.3 9.1,7.3" fill="none" stroke="#0038b8" stroke-width="0.95" />
      {:else if normalized === 'za'}
        <rect width="24" height="9" fill="#e03c31" />
        <rect y="9" width="24" height="9" fill="#001489" />
        <polygon points="0,0 4.5,0 12.2,6.2 24,6.2 24,11.8 12.2,11.8 4.5,18 0,18" fill="#fff" />
        <polygon points="0,0 2.6,0 11.4,7.2 24,7.2 24,10.8 11.4,10.8 2.6,18 0,18" fill="#007749" />
        <polygon points="0,2.4 8,9 0,15.6" fill="#ffb81c" />
        <polygon points="0,4.2 5.8,9 0,13.8" fill="#000" />
      {:else if normalized === 'mx'}
        <rect width="8" height="18" fill="#006847" />
        <rect x="8" width="8" height="18" fill="#fff" />
        <rect x="16" width="8" height="18" fill="#ce1126" />
        <circle cx="12" cy="9" r="1.8" fill="#8b5a2b" />
        <path d="M10.4 10.4A2.2 2.2 0 0 0 13.6 10.4" fill="none" stroke="#006847" stroke-width="0.8" />
      {:else if normalized === 'nz'}
        <rect width="24" height="18" fill="#00247d" />
        <svg x="0" y="0" width="12" height="9" viewBox="0 0 24 18">
          <rect width="24" height="18" fill="#00247d" />
          <path d="M0 0L24 18M24 0L0 18" stroke="#fff" stroke-width="4" />
          <path d="M0 0L24 18M24 0L0 18" stroke="#cc142b" stroke-width="1.8" />
          <path d="M12 0V18M0 9H24" stroke="#fff" stroke-width="6.4" />
          <path d="M12 0V18M0 9H24" stroke="#cc142b" stroke-width="3.6" />
        </svg>
        <g fill="#cc142b" stroke="#fff" stroke-width="0.55">
          <circle cx="18" cy="4.2" r="1.05" />
          <circle cx="21" cy="7.6" r="0.95" />
          <circle cx="15.2" cy="8.2" r="0.95" />
          <circle cx="18" cy="14" r="1.15" />
        </g>
      {:else if normalized === 'be'}
        <rect width="8" height="18" fill="#000" />
        <rect x="8" width="8" height="18" fill="#fae042" />
        <rect x="16" width="8" height="18" fill="#ed2939" />
      {:else if normalized === 'at'}
        <rect width="24" height="6" fill="#ed2939" />
        <rect y="6" width="24" height="6" fill="#fff" />
        <rect y="12" width="24" height="6" fill="#ed2939" />
      {:else if normalized === 'cz'}
        <rect width="24" height="9" fill="#fff" />
        <rect y="9" width="24" height="9" fill="#d7141a" />
        <polygon points="0,0 11.5,9 0,18" fill="#11457e" />
      {:else if normalized === 'dk'}
        <rect width="24" height="18" fill="#c60c30" />
        <rect x="7" width="3" height="18" fill="#fff" />
        <rect y="7.5" width="24" height="3" fill="#fff" />
      {:else if normalized === 'pt'}
        <rect width="9.6" height="18" fill="#006600" />
        <rect x="9.6" width="14.4" height="18" fill="#ff0000" />
        <circle cx="9.6" cy="9" r="3.1" fill="#ffcc00" />
        <rect x="8.2" y="7.3" width="2.8" height="3.4" rx="0.9" fill="#ff0000" />
        <rect x="8.9" y="8" width="1.4" height="2" rx="0.4" fill="#fff" />
      {:else if normalized === 'my'}
        <rect width="24" height="18" fill="#fff" />
        <g fill="#cc0001">
          <rect y="0" width="24" height="1.286" />
          <rect y="2.571" width="24" height="1.286" />
          <rect y="5.143" width="24" height="1.286" />
          <rect y="7.714" width="24" height="1.286" />
          <rect y="10.286" width="24" height="1.286" />
          <rect y="12.857" width="24" height="1.286" />
          <rect y="15.429" width="24" height="1.286" />
        </g>
        <rect width="12" height="10.286" fill="#010066" />
        <circle cx="5" cy="5.15" r="2.9" fill="#ffcc00" />
        <circle cx="5.95" cy="5.15" r="2.35" fill="#010066" />
        <polygon
          fill="#ffcc00"
          points="8.5,2.9 8.95,4.35 10.45,4.35 9.25,5.2 9.7,6.65 8.5,5.8 7.3,6.65 7.75,5.2 6.55,4.35 8.05,4.35" />
      {:else if normalized === 'th'}
        <rect width="24" height="3" fill="#a51931" />
        <rect y="3" width="24" height="3" fill="#f4f5f8" />
        <rect y="6" width="24" height="6" fill="#2d2a4a" />
        <rect y="12" width="24" height="3" fill="#f4f5f8" />
        <rect y="15" width="24" height="3" fill="#a51931" />
      {:else if normalized === 'vn'}
        <rect width="24" height="18" fill="#da251d" />
        <polygon
          fill="#ff0"
          points="12,4 13.4,8.2 17.8,8.2 14.2,10.8 15.6,15 12,12.4 8.4,15 9.8,10.8 6.2,8.2 10.6,8.2" />
      {:else if normalized === 'id'}
        <rect width="24" height="9" fill="#ce1126" />
        <rect y="9" width="24" height="9" fill="#fff" />
      {:else if normalized === 'ph'}
        <rect width="24" height="9" fill="#0038a8" />
        <rect y="9" width="24" height="9" fill="#ce1126" />
        <polygon points="0,0 10.8,9 0,18" fill="#fff" />
        <g fill="#fcd116">
          <circle cx="3.9" cy="9" r="1.85" />
          <circle cx="1.6" cy="3.2" r="0.65" />
          <circle cx="1.6" cy="14.8" r="0.65" />
          <circle cx="8.3" cy="9" r="0.65" />
        </g>
      {:else if normalized === 'ro'}
        <rect width="8" height="18" fill="#002b7f" />
        <rect x="8" width="8" height="18" fill="#fcd116" />
        <rect x="16" width="8" height="18" fill="#ce1126" />
      {:else if normalized === 'bg'}
        <rect width="24" height="6" fill="#fff" />
        <rect y="6" width="24" height="6" fill="#00966e" />
        <rect y="12" width="24" height="6" fill="#d62612" />
      {:else if normalized === 'hu'}
        <rect width="24" height="6" fill="#cd2a3e" />
        <rect y="6" width="24" height="6" fill="#fff" />
        <rect y="12" width="24" height="6" fill="#436f4d" />
      {:else if normalized === 'gr'}
        <rect width="24" height="18" fill="#fff" />
        <g fill="#0d5eaf">
          <rect y="0" width="24" height="2" />
          <rect y="4" width="24" height="2" />
          <rect y="8" width="24" height="2" />
          <rect y="12" width="24" height="2" />
          <rect y="16" width="24" height="2" />
          <rect width="10" height="10" />
        </g>
        <rect x="4" width="2" height="10" fill="#fff" />
        <rect y="4" width="10" height="2" fill="#fff" />
      {:else if normalized === 'lu'}
        <rect width="24" height="6" fill="#ed2939" />
        <rect y="6" width="24" height="6" fill="#fff" />
        <rect y="12" width="24" height="6" fill="#00a1de" />
      {:else if normalized === 'is'}
        <rect width="24" height="18" fill="#02529c" />
        <rect x="6.4" width="4.4" height="18" fill="#fff" />
        <rect y="6.8" width="24" height="4.4" fill="#fff" />
        <rect x="7.5" width="2.2" height="18" fill="#dc1e35" />
        <rect y="7.9" width="24" height="2.2" fill="#dc1e35" />
      {:else if normalized === 'ar'}
        <rect width="24" height="6" fill="#74acdf" />
        <rect y="6" width="24" height="6" fill="#fff" />
        <rect y="12" width="24" height="6" fill="#74acdf" />
        <circle cx="12" cy="9" r="1.9" fill="#f6b40e" />
      {:else if normalized === 'cl'}
        <rect width="24" height="9" fill="#fff" />
        <rect y="9" width="24" height="9" fill="#d52b1e" />
        <rect width="9" height="9" fill="#0039a6" />
        <polygon
          fill="#fff"
          points="4.5,2.2 5.1,4.0 7.0,4.0 5.45,5.1 6.05,6.9 4.5,5.8 2.95,6.9 3.55,5.1 2.0,4.0 3.9,4.0" />
      {:else if normalized === 'co'}
        <rect width="24" height="9" fill="#fcd116" />
        <rect y="9" width="24" height="4.5" fill="#003893" />
        <rect y="13.5" width="24" height="4.5" fill="#ce1126" />
      {/if}
    </svg>
  </span>
{:else if normalized}
  <span
    role="img"
    aria-label={upperCode}
    class={cn(
      'inline-flex h-3.5 min-w-5 shrink-0 items-center justify-center rounded-[2px] border border-border bg-muted px-1 font-technical text-[9px] leading-none font-medium text-muted-foreground uppercase',
      className,
    )}>
    {upperCode}
  </span>
{:else}
  <GlobeIcon class={cn('size-3.5 shrink-0 text-muted-foreground', className)} aria-hidden="true" />
{/if}
