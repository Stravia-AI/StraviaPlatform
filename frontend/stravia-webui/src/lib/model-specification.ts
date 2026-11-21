import * as m from '#lib/paraglide/messages.js'
import AudioLinesIcon from '@lucide/svelte/icons/audio-lines'
import FileQuestionIcon from '@lucide/svelte/icons/file-question'
import FileTextIcon from '@lucide/svelte/icons/file-text'
import ImageIcon from '@lucide/svelte/icons/image'
import TypeIcon from '@lucide/svelte/icons/type'
import VideoIcon from '@lucide/svelte/icons/video'

import { formatNumber } from '#lib/format.js'

const BINARY_K = 1_024
const BINARY_M = BINARY_K * BINARY_K

export function formatSpecificationTokens(value: number): string {
  if (value >= 1_000_000 && value % 10_000 === 0) return `${value / 1_000_000}M`
  if (value >= BINARY_M && value % (BINARY_M / 4) === 0) return `${value / BINARY_M}M`
  if (value >= 1_000 && value % 1_000 === 0) return `${value / 1_000}K`
  if (value >= BINARY_K && value % BINARY_K === 0) return `${value / BINARY_K}K`
  if (value >= 1_000 && value % 10 === 0) return `${value / 1_000}K`
  return formatNumber(value)
}

export const specificationReasoningEfforts = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const

export const specificationModalities = [
  { key: 'text', label: m.model_specification_modality_text, icon: TypeIcon },
  { key: 'image', label: m.model_specification_modality_image, icon: ImageIcon },
  { key: 'audio', label: m.model_specification_modality_audio, icon: AudioLinesIcon },
  { key: 'video', label: m.model_specification_modality_video, icon: VideoIcon },
  { key: 'pdf', label: m.model_specification_modality_pdf, icon: FileTextIcon },
] as const

export function specificationModality(modality: string) {
  const normalized = modality.toLocaleLowerCase()
  const known = specificationModalities.find(({ key }) => key === normalized)
  return known ?? { key: modality, label: () => modality, icon: FileQuestionIcon }
}
