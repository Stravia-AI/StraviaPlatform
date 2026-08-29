export function modelIdFromCatalogId(catalogId: string): string {
  const separator = catalogId.indexOf('/')
  return separator === -1 ? catalogId : catalogId.slice(separator + 1)
}
