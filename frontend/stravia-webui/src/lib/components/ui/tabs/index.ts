import Content from './tabs-content.svelte'
import Trigger from './tabs-trigger.svelte'
import Root from './tabs.svelte'
import List from './tabs-list.svelte'
import { tabsListVariants, tabsTriggerVariants, type TabsListVariant } from './variants.js'

export {
  Root,
  Content,
  List,
  Trigger,
  tabsListVariants,
  tabsTriggerVariants,
  type TabsListVariant,
  //
  Root as Tabs,
  Content as TabsContent,
  List as TabsList,
  Trigger as TabsTrigger,
}
