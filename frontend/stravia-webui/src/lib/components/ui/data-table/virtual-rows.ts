export interface DataTableVirtualRange {
  startIndex: number
  endIndex: number
  topPadding: number
  bottomPadding: number
}

interface DataTableVirtualRangeInput {
  enabled: boolean
  rowCount: number
  scrollTop: number
  viewportHeight: number
  itemSize: number
  overscan: number
}

export function dataTableVirtualRange({
  enabled,
  rowCount,
  scrollTop,
  viewportHeight,
  itemSize,
  overscan,
}: DataTableVirtualRangeInput): DataTableVirtualRange {
  if (!enabled) {
    return { startIndex: 0, endIndex: rowCount, topPadding: 0, bottomPadding: 0 }
  }

  const safeItemSize = Math.max(1, itemSize)
  const startIndex = Math.max(0, Math.floor(scrollTop / safeItemSize) - overscan)
  const endIndex = Math.min(rowCount, Math.ceil((scrollTop + viewportHeight) / safeItemSize) + overscan)
  return {
    startIndex,
    endIndex,
    topPadding: startIndex * safeItemSize,
    bottomPadding: (rowCount - endIndex) * safeItemSize,
  }
}
