import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

export type WithElementRef<T> = T & { ref?: HTMLElement | null }
export type WithoutChild<T> = Omit<T, 'child'>
export type WithoutChildren<T> = Omit<T, 'children'>
export type WithoutChildrenOrChild<T> = WithoutChildren<WithoutChild<T>>

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}
