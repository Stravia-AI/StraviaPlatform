/** `SearchCombobox` 的候选项。 */
export interface SearchComboboxOption {
  /** 选中后写回的值，例如 Route、供应商或模型 ID。 */
  value: string
  /** 第一行展示名。 */
  label: string
  /** 第二行等宽技术标识；与展示名相同时省略。 */
  detail?: string
  /** 只参与搜索、不显示的词。 */
  keywords?: string[]
  /** 展示名本身就是技术标识（未命名时回退为 ID），按等宽字显示。 */
  technical?: boolean
  disabled?: boolean
}

/** `{ id, name }` 形状的目录项：名称缺失或与 ID 相同时只显示一次 ID。 */
export function idNameOption(item: { id: string; name?: string | null }): SearchComboboxOption {
  const name = item.name?.trim()
  return name && name !== item.id
    ? { value: item.id, label: name, detail: item.id }
    : { value: item.id, label: item.id, technical: true }
}
