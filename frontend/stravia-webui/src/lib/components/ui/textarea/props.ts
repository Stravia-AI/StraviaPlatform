import type { HTMLTextareaAttributes } from 'svelte/elements'

import type { WithElementRef, WithoutChildren } from '$lib/utils.js'

export type TextareaProps = WithoutChildren<WithElementRef<HTMLTextareaAttributes>>
