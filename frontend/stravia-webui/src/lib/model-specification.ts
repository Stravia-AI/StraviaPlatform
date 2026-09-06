import * as m from '$lib/paraglide/messages.js'
import AudioLinesIcon from '@lucide/svelte/icons/audio-lines'
import BrainCircuitIcon from '@lucide/svelte/icons/brain-circuit'
import BracesIcon from '@lucide/svelte/icons/braces'
import FileQuestionIcon from '@lucide/svelte/icons/file-question'
import FileTextIcon from '@lucide/svelte/icons/file-text'
import ImageIcon from '@lucide/svelte/icons/image'
import PaperclipIcon from '@lucide/svelte/icons/paperclip'
import ThermometerIcon from '@lucide/svelte/icons/thermometer'
import TypeIcon from '@lucide/svelte/icons/type'
import VideoIcon from '@lucide/svelte/icons/video'
import WrenchIcon from '@lucide/svelte/icons/wrench'

import { formatNumber } from '$lib/format'

export function formatSpecificationTokens(value: number): string {
  if (value >= 1_000_000 && value % 10_000 === 0) return `${value / 1_000_000}M`
  if (value >= 1_000 && value % 10 === 0) return `${value / 1_000}K`
  return formatNumber(value)
}

export const specificationFeatures = [
  { key: 'reasoning', label: m.model_specification_reasoning, icon: BrainCircuitIcon },
  { key: 'tool_call', label: m.model_specification_tool_calls, icon: WrenchIcon },
  { key: 'structured_output', label: m.model_specification_structured_output, icon: BracesIcon },
  { key: 'attachment', label: m.model_specification_attachments, icon: PaperclipIcon },
  { key: 'temperature', label: m.model_specification_temperature, icon: ThermometerIcon },
] as const

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
