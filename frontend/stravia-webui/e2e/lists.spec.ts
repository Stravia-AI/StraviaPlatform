import { expect, test } from '@playwright/test'
import type { Locator, Page } from '@playwright/test'

import type { CredentialRule, CustomCredentialRule } from '../src/lib/types'
import { prepareApp } from './prepare-app'

function rule(index: number): CredentialRule {
  return {
    id: `rule-${index}`,
    name: `Provider token ${index}`,
    target: 'token',
    description: `Matches provider token ${index}.`,
    regex: 'tok_[a-z0-9]{16}',
    path: null,
    secret_group: 0,
    keywords: [`tok${index}`],
    filter: '',
    components: [],
    skip_report: false,
    specificity: 1,
    confidence: 'high',
  }
}

const customRule: CustomCredentialRule = {
  id: 'custom-1',
  name: 'Internal token',
  description: '',
  enabled: true,
  spec: { mode: 'simple', text: 'internal-secret' },
  created_at: 1,
  updated_at: 1,
}

async function mockRedaction(page: Page) {
  await page.route('**/api/v1/reversible-redaction/rules', (route) =>
    route.fulfill({
      json: { data: { rules: Array.from({ length: 12 }, (_, index) => rule(index)), prefilter: '', filter: '' } },
    }),
  )
  await page.route('**/api/v1/reversible-redaction/discoveries?**', (route) =>
    route.fulfill({ json: { data: { items: [], next_cursor: null, observation_gap: false } } }),
  )
  await page.route('**/api/v1/reversible-redaction/custom-rules', (route) =>
    route.fulfill({ json: { data: [customRule] } }),
  )
}

function overlaps(a: { x: number; y: number; width: number; height: number }, b: typeof a): boolean {
  return (
    a.x < b.x + b.width - 0.5 && b.x < a.x + a.width - 0.5 && a.y < b.y + b.height - 0.5 && b.y < a.y + a.height - 0.5
  )
}

async function boxes(locators: Locator[]) {
  return Promise.all(
    locators.map(async (locator) => {
      const box = await locator.boundingBox()
      if (!box) throw new Error('Toolbar control is not visible')
      return box
    }),
  )
}

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
  await mockRedaction(page)
})

for (const width of [390, 320]) {
  test(`DataTable toolbar puts search on its own row without overlap at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 800 })
    await page.goto('/reversible-redaction')
    await page.getByRole('tab', { name: /Current rules/ }).click()
    const toolbar = page.locator('[data-slot="data-table-toolbar"]')
    await expect(toolbar).toBeVisible()
    const search = toolbar.locator('[data-slot="input-group"]').first()
    const filter = toolbar.getByLabel('Rule source')
    const [toolbarBox, searchBox, filterBox] = await boxes([toolbar, search, filter])

    // 搜索整行且在最前；筛选排在其后。
    expect(searchBox.width).toBeGreaterThan(toolbarBox.width - 2)
    expect(filterBox.y).toBeGreaterThanOrEqual(searchBox.y + searchBox.height - 0.5)
    expect(overlaps(searchBox, filterBox)).toBe(false)
    for (const box of [searchBox, filterBox]) {
      expect(box.x).toBeGreaterThanOrEqual(toolbarBox.x - 0.5)
      expect(box.x + box.width).toBeLessThanOrEqual(toolbarBox.x + toolbarBox.width + 0.5)
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  })
}

test('DataTable toolbar keeps search and actions on one row when the table is wide', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/reversible-redaction')
  await page.getByRole('tab', { name: /Current rules/ }).click()
  const toolbar = page.locator('[data-slot="data-table-toolbar"]')
  const [searchBox, filterBox] = await boxes([toolbar.locator('input').first(), toolbar.getByLabel('Rule source')])
  expect(Math.abs(searchBox.y - filterBox.y)).toBeLessThan(searchBox.height)
  expect(searchBox.x).toBeGreaterThan(filterBox.x)
})

test('workbench list scrolls inside the remaining height and keeps the paginator reachable', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 700 })
  await page.goto('/reversible-redaction')
  await page.getByRole('tab', { name: /Current rules/ }).click()
  const viewport = page.locator('[data-slot="data-table-viewport"] [data-scroll-area-viewport]')
  await expect(viewport).toBeVisible()
  const next = page.getByRole('button', { name: 'Next', exact: true })
  // 数据多：行数超出剩余高度时在表格内部滚动，分页仍在视口内，整页不滚动。
  expect(await viewport.evaluate((element) => element.scrollHeight > element.clientHeight)).toBe(true)
  await expect(next).toBeInViewport()
  expect(await page.evaluate(() => document.documentElement.scrollHeight <= innerHeight + 1)).toBe(true)

  // 数据少：自然高度，不被拉伸去填满剩余高度。
  const tall = (await viewport.boundingBox())!.height
  await page.getByLabel('Rule source').click()
  await page.getByRole('option', { name: 'Custom rules' }).click()
  await expect(page.getByRole('row')).toHaveCount(2)
  expect((await viewport.boundingBox())!.height).toBeLessThan(tall / 2)
})

test('custom rule Sheet confirms before discarding a draft and closes directly without one', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/reversible-redaction')
  await page.getByRole('tab', { name: /Current rules/ }).click()
  const sheet = page.locator('[data-slot="sheet-content"]')
  const dialog = page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })

  await page.getByRole('row').filter({ hasText: 'Internal token' }).click()
  await expect(sheet).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(sheet).toBeHidden()
  await expect(dialog).toHaveCount(0)

  await page.getByRole('row').filter({ hasText: 'Internal token' }).click()
  const name = sheet.getByLabel('Rule name')
  await name.fill('Renamed rule')
  await page.keyboard.press('Escape')
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Keep editing' }).click()
  await expect(dialog).toHaveCount(0)
  await expect(name).toHaveValue('Renamed rule')

  await page.mouse.click(8, 400)
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Discard changes' }).click()
  await expect(sheet).toBeHidden()
})
