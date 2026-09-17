import type { HTMLInputAttributes, HTMLInputTypeAttribute } from 'svelte/elements'

import type { WithElementRef } from '$lib/utils.js'

export type InputType = Exclude<HTMLInputTypeAttribute, 'file'>

export type InputProps = WithElementRef<
  Omit<HTMLInputAttributes, 'type' | 'value'> &
    ({ type: 'file'; files?: FileList; value?: unknown } | { type?: InputType; files?: undefined; value?: unknown })
>
