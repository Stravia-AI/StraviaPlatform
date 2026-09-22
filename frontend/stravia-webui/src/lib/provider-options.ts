import type { ProviderDescriptor, VendorChannelDescriptor } from '$lib/types'

export interface ProviderOption {
  key: string
  descriptor: ProviderDescriptor
  channel: VendorChannelDescriptor
}

export function buildProviderOptions(descriptors: ProviderDescriptor[]): ProviderOption[] {
  return descriptors.flatMap((descriptor) =>
    descriptor.channels.map((channel) => ({ key: `${descriptor.provider_id}/${channel.id}`, descriptor, channel })),
  )
}

export function optionLabel(option: ProviderOption): string {
  return option.descriptor.display_name
}

export function optionDescription(option: ProviderOption): string {
  const details = option.channel.description || option.channel.capabilities.join(' · ')
  return option.descriptor.channels.length > 1 ? `${option.channel.name} · ${details}` : details
}

export function defaultProviderName(option: ProviderOption): string {
  return option.descriptor.channels.length > 1
    ? `${option.descriptor.display_name} ${option.channel.name}`
    : option.descriptor.display_name
}

export function providerNameAfterOptionChange(
  currentName: string,
  previousOption: ProviderOption | undefined,
  nextOption: ProviderOption,
): string {
  const trimmedName = currentName.trim()
  const previousDefault = previousOption ? defaultProviderName(previousOption) : ''
  return trimmedName && trimmedName !== previousDefault ? trimmedName : defaultProviderName(nextOption)
}
