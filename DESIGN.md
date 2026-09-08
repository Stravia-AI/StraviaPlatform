---
version: alpha
name: Stravia 本地仪表控制台
description: 面向个人开发者的本地 AI 网关工作台，以轻量任务衔接、准确文案和常规操作反馈帮助用户接入模型与客户端。
colors:
  primary: "oklch(0.4693 0.0775 244.17)"
  primary-foreground: "oklch(1 0 0)"
  background: "oklch(0.97 0.0025 228.86)"
  foreground: "oklch(0.2159 0.0075 248.17)"
  surface: "oklch(1 0 0)"
  popover: "oklch(1 0 0)"
  secondary: "oklch(0.948 0.004 228.86)"
  muted: "oklch(0.944 0.006 228.86)"
  muted-foreground: "oklch(0.5395 0.0174 242)"
  accent: "oklch(0.925 0.018 241)"
  accent-foreground: "oklch(0.375 0.071 244)"
  destructive: "oklch(0.509 0.1161 32.92)"
  destructive-foreground: "oklch(1 0 0)"
  success: "oklch(0.4805 0.0743 164.75)"
  warning: "oklch(0.5257 0.0968 76.39)"
  signal: "oklch(0.58 0.132 48)"
  border: "oklch(0.8969 0.0077 228.86)"
  input: "oklch(0.865 0.009 230)"
  sidebar: "oklch(0.925 0.012 241)"
  sidebar-foreground: "oklch(0.2159 0.0075 248.17)"
  sidebar-accent: "oklch(0.865 0.035 241)"
  sidebar-accent-foreground: "oklch(0.345 0.076 244)"
  sidebar-border: "oklch(0.82 0.018 241)"
  chart-1: "oklch(0.4693 0.0775 244.17)"
  chart-2: "oklch(0.57 0.066 241)"
  chart-3: "oklch(0.66 0.054 239)"
  chart-4: "oklch(0.76 0.041 237)"
  chart-5: "oklch(0.509 0.1161 32.92)"
  primary-dark: "oklch(0.7329 0.066 238.9)"
  primary-foreground-dark: "oklch(0.1893 0.0077 248.23)"
  background-dark: "oklch(0.17 0.006 248.23)"
  foreground-dark: "oklch(0.9561 0.0035 219.53)"
  surface-dark: "oklch(0.2274 0.0108 242.21)"
  secondary-dark: "oklch(0.278 0.012 241)"
  muted-dark: "oklch(0.278 0.012 241)"
  muted-foreground-dark: "oklch(0.7218 0.0161 235.48)"
  accent-dark: "oklch(0.302 0.03 241)"
  accent-foreground-dark: "oklch(0.835 0.052 239)"
  destructive-dark: "oklch(0.688 0.11 31.35)"
  destructive-foreground-dark: "oklch(0.1893 0.0077 248.23)"
  success-dark: "oklch(0.7254 0.0833 162.7)"
  warning-dark: "oklch(0.742 0.1072 76.3)"
  signal-dark: "oklch(0.72 0.12 48)"
  border-dark: "oklch(0.3395 0.0151 240.38)"
  input-dark: "oklch(0.382 0.017 240)"
  sidebar-dark: "oklch(0.235 0.013 242.21)"
  sidebar-foreground-dark: "oklch(0.9561 0.0035 219.53)"
  sidebar-accent-dark: "oklch(0.33 0.038 241)"
  sidebar-accent-foreground-dark: "oklch(0.86 0.056 239)"
  sidebar-border-dark: "oklch(0.37 0.018 240.38)"
typography:
  body:
    fontFamily: "IBM Plex Sans, Noto Sans SC Variable, Noto Sans SC, system-ui, sans-serif"
    fontSize: 14px
    fontWeight: 400
    lineHeight: 1.5
  body-chinese:
    fontFamily: "Noto Sans SC Variable, Noto Sans SC, IBM Plex Sans, sans-serif"
    fontSize: 14px
    fontWeight: 400
    lineHeight: 1.58
  page-title:
    fontFamily: "IBM Plex Sans Condensed, IBM Plex Sans, sans-serif"
    fontSize: 30px
    fontWeight: 600
    lineHeight: 1.15
    letterSpacing: -0.025em
  page-title-compact:
    fontFamily: "IBM Plex Sans Condensed, IBM Plex Sans, sans-serif"
    fontSize: 26px
    fontWeight: 600
    lineHeight: 1.15
    letterSpacing: -0.025em
  section-title:
    fontFamily: "IBM Plex Sans Condensed, IBM Plex Sans, sans-serif"
    fontSize: 18px
    fontWeight: 600
    lineHeight: 1.35
  eyebrow:
    fontFamily: "IBM Plex Sans Condensed, IBM Plex Sans, sans-serif"
    fontSize: 11.52px
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: 0.14em
  navigation:
    fontFamily: "IBM Plex Sans, Noto Sans SC Variable, Noto Sans SC, sans-serif"
    fontSize: 13px
    fontWeight: 500
    lineHeight: 1.4
  technical:
    fontFamily: "IBM Plex Mono, ui-monospace, monospace"
    fontSize: 12px
    fontWeight: 400
    lineHeight: 1.5
    fontFeature: "'tnum' 1"
rounded:
  sm: 4px
  md: 6px
  lg: 8px
  xl: 12px
  full: 9999px
spacing:
  unit: 4px
  xs: 4px
  sm: 8px
  md: 16px
  lg: 24px
  xl: 32px
  control-gap: 12px
  titlebar-height: 40px
  sidebar-compact-width: 48px
  sidebar-width: 256px
  content-max-width: 1800px
  settings-max-width: 64rem
  editor-max-width: 90rem
components:
  app-shell:
    backgroundColor: "{colors.background}"
    textColor: "{colors.foreground}"
  sidebar:
    backgroundColor: "{colors.sidebar}"
    textColor: "{colors.sidebar-foreground}"
    width: "{spacing.sidebar-width}"
  sidebar-compact:
    backgroundColor: "{colors.sidebar}"
    textColor: "{colors.sidebar-foreground}"
    width: "{spacing.sidebar-compact-width}"
  navigation-active:
    backgroundColor: "{colors.sidebar-accent}"
    textColor: "{colors.sidebar-accent-foreground}"
    rounded: "{rounded.md}"
    height: 40px
    padding: "0 12px"
  button-primary:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.primary-foreground}"
    rounded: "{rounded.lg}"
    height: 40px
    padding: "0 12px"
  button-large:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.primary-foreground}"
    rounded: "{rounded.lg}"
    height: 44px
    padding: "0 16px"
  icon-button:
    rounded: "{rounded.lg}"
    size: 40px
  input:
    backgroundColor: "{colors.background}"
    textColor: "{colors.foreground}"
    rounded: "{rounded.lg}"
    height: 40px
  field-help-trigger:
    textColor: "{colors.muted-foreground}"
    rounded: "{rounded.md}"
    size: 40px
  field-number:
    width: 7rem
  field-datetime:
    width: 18rem
  field-name:
    width: 24rem
  field-select:
    width: 28rem
  field-fill:
    width: 36rem
  route-spine:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.foreground}"
    rounded: "{rounded.xl}"
    padding: 16px
  provider-mark:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.primary}"
    rounded: "{rounded.md}"
    size: 28px
  overlay:
    backgroundColor: "{colors.popover}"
    textColor: "{colors.foreground}"
    width: 32rem
  overlay-medium:
    backgroundColor: "{colors.popover}"
    textColor: "{colors.foreground}"
    width: 40rem
  provider-overlay:
    backgroundColor: "{colors.popover}"
    textColor: "{colors.foreground}"
    width: 56rem
  tooltip:
    backgroundColor: "{colors.foreground}"
    textColor: "{colors.background}"
    rounded: "{rounded.md}"
    padding: "6px 12px"
---

# Stravia 前端设计系统

> 本文件是 Stravia WebUI 的设计规范，沿用 YAML token 与正文分区的结构。YAML 保留视觉基础，正文规定设计、实现与验收要求，不代表每条要求已实现。主题值以 `frontend/stravia-webui/src/app.css` 为实现来源，组件行为以现有共享组件为来源；改动时同步规范，不另建平行设计系统。正文中的“必须”“不得”是约束，“优先”“默认”允许基于真实任务说明例外。

## Overview

Stravia 是本地运行、可自托管的 AI 协议网关。管理界面优先服务个人开发者：连接所需模型服务，为 Codex、Claude Code 等第三方工具配置模型访问，并在需要时查看用量与请求记录。既有管理能力保留，但不以多人密钥分配或批量运维组织主流程。界面首先帮助用户回答：**现在该做什么、客户端可以用什么模型、操作是否完成、出问题后怎么继续**。

视觉参考不是营销型 SaaS 仪表盘，而是**桌边网络网关的控制面板与实验室仪表读数**：左侧像设备目录，主区域像规整的操作记录板，请求路径像配线面板，状态标记像小型信号灯。这个参考世界决定以下性格：

- **精密但不压迫。** 信息可以密集，结构必须平静；让边线、对齐和字体层级承担组织职责。
- **技术但不晦涩。** ID、URL、协议、时间和计数保持工程精度；说明文案围绕用户目标、动作与可观察结果。
- **本地且可信。** 不用夸张的品牌舞台、云端意象或“智能魔法”装饰；优先展示真实状态与恢复动作。
- **克制但不单调。** 低彩度钢蓝建立秩序；绿色、琥珀和红色仅在状态需要时出现。
- **桌面优先、移动可用。** 宽屏容纳表格、编辑器和分析图；窄屏重排为列表与抽屉，不缩小成不可操作的桌面截图。

当本文件没有规定具体样式时，按以下顺序决策：可读性与任务完成 > 状态可辨识性 > 与现有组件一致 > 信息密度 > 装饰性。Stravia 的视觉签名是**低彩度仪表底色上的 Request Spine（请求路径）**，而不是渐变、玻璃光效或大面积插画。

## Product Interaction

### 任务主线与渐进披露

默认流程是**连接模型服务 → 搜索并添加模型 → 接入客户端 → 复用已有密钥 → 复制或写入配置**。它表达用户任务，不要求按顺序重新创建已有资源，也不是新的持久化向导。

- 保留现有导航分组、页面骨架与资源的唯一编辑表面。跨页引导复用这些入口，不复制表单或增加强制向导。
- 配置不足时，在概览顶部用一张轻量任务卡突出当前推荐操作。先补连接服务与添加模型的依赖，再补接入所需密钥；已有资源直接复用。
- 具备接入所需配置后，以常规概览为主，保留「接入客户端」快捷入口。没有请求时用简短空态解释，不堆叠多个空排行，也不要求先发请求才能离开引导。
- Request Spine 解释 API Key → Model → Provider 的请求流向，不承担配置步骤编号或完成进度。
- 列表和默认路径先满足直接使用；自定义 Model ID、多服务 Target 与优先级在需要时通过现有编辑器配置。不得让首次接入依赖用户先理解 Route 或 Target。
- 用量、额度与请求记录是日常查看和排查入口，不抢占首次配置的主要动作。

### 模型服务与添加模型

- Provider 保存后沿用进入详情并同步 Provider Model 的流程。同步期间可以继续使用页面；失败时分别说明连接配置已保存与模型同步失败，并提供重试。
- 默认从服务的模型清单搜索并直接添加，沿用上游模型 ID 作为 Route ID；按用途命名是自定义路径，不是前置步骤。
- 同一 Route ID 已存在时，沿用既有绑定语义加入 Target，不创建重复 Route。按钮区分「添加模型」与「添加到现有模型」，反馈说明真实结果。
- 添加动作始终可识别，不能用「尚未使用」等状态词代替操作名称，也不能依赖 hover 才揭示关键动作。
- 添加成功后提供「接入客户端」，同时允许继续添加其他模型，不强制跳页。
- 模型列表优先呈现 Model Display Name、客户端使用的 Model ID、启用状态与关联服务。名称为空时按既有规则回退到 Route ID；展示名称不能替代调用身份。
- 保留 Route 编辑器的优先级分层与禁用备用区，不改为自由流程图，不改变 Target 选择策略。

### 功能页面是工作台，不是实现说明书

- 首屏直接呈现最常用任务及其数据，不以原理说明、技术保证、状态汇总或大段介绍占据主要空间。默认入口按用户最常做的事选择，不按后端模块顺序排列。
- 同一能力中的浏览、记录和手动测试属于不同任务时，使用同级 Tabs 分开；不把长表格、历史记录和测试表单纵向堆满一页。凭据保护以规则浏览为默认页签，命中记录和匹配测试分别提供独立工作区。
- 宽屏测试工作区可将输入与结果并排，窄屏按“输入 → 操作 → 结果”堆叠。输入、结果、加载和失败状态必须有明确归属，不能为并排而牺牲编辑区宽度。
- 能由页签、表头、输入标签直接说明的内容，不再重复一遍 section 标题、介绍或角标。保留必要的语义标题；纯视觉重复时可以使用 `sr-only`，不能删除可访问名称。
- 详情采用渐进披露：列表帮助识别与选择，Inspector 帮助理解条件，高级参数按需展开。不得把完整详情复制到每一行，也不得让查看基础数据先经过说明页。

### 接入客户端与密钥复用

- 接入页突出客户端选择、密钥选择、配置结果和复制或写入操作。保留代码示例路径，按需显示协议与模型字段。
- 默认复用已有 API Key：只有一个符合当前接入条件的候选时默认选中；多个时由用户选择；没有时提供既有创建或管理入口。允许主动创建独立密钥，但不强制每个客户端分别创建。
- 候选资格遵循既有启用、有效期与模型访问规则，不自动扩大权限，不在 WebUI 新建一套授权规则或权限向导。
- 用户已作出的有效选择优先于自动选择。补齐资源后返回接入页时保留此前仍有效的选择；不为此持久化 secret 或新增全局流程状态。
- 缺少前置条件时就地给出最短补齐入口，完成后提供「继续接入」。跨页串联不产生第二套资源编辑表面。
- Stravia Desktop 以写入配置为主操作并保留复制；独立服务端只提供复制，不写用户本机文件。
- Connect Client Apply 沿用增量写入契约，不选择当前或默认模型；Claude Code 四套模型映射是既有例外，不能为简化界面而省略。

### 常规反馈，不做过度防御

- 复制成功显示「已复制」，Desktop 写入成功显示「配置已写入」，当前操作就此完成。不把生成、复制或写入配置表述成客户端请求成功。
- 不建立首次成功请求状态机，不自动发送可能收费的上游验证请求，不要求用户再次确认已经完成的低风险操作。
- 已配置、已启用、检查成功与请求成功是不同事实。资源总数不能直接标成可用数量；未知和未加载状态保持中性。
- 禁用操作附近说明当前阻碍与下一步，不只降低透明度。必要原因不能只藏在 hover、Tooltip 或默认折叠的高级区域。
- 保存失败保留输入；部分成功时说明哪些结果已经保存、哪一步仍需处理，不让用户重填无关配置。
- 复杂编辑有未保存变更时使用常规离开提示；没有变更时不打扰。正常添加、保存、复制不增加确认；破坏性操作沿用必要确认和影响说明。
- 立即生效与显式保存的操作应可区分。离开草稿不应让用户误以为已立即生效的改动也会撤销。
- **能独立、完整提交的操作默认即时保存。** 启停开关、独立偏好与已具备完整值的选择，不再附加一次“保存”点击；保存中提供就地反馈并阻止重复提交，失败保留服务器确认值并给出可恢复错误。不得用每次输入即提交的方式保存半成品。
- 多字段依赖、组合校验、资源创建与凭据更新保留明确提交。即时启停仅改变已保存配置的启用状态，不顺带提交或清空旁边的表单草稿；未满足已保存前置条件时，说明先完成配置，而不是让用户反复尝试。
- 开关已表达当前状态、表单没有变更时，不常驻重复的“已保存策略：关闭”等反馈条。加载、未保存、保存中和失败按需出现；已保存值与草稿不同必须明确提示，加载失败不能回退成看似有效的“关闭”。

### 面向用户的术语与文案

领域含义遵循 `CONTEXT.md`。下面规定展示方式，不重定义后端概念：

| 概念或场景 | 展示规范 |
| --- | --- |
| Connect Client | 使用「客户端 / Client」，必要时直接写 Codex、Claude Code；不称为 Agent，不与 Stravia Desktop 混用。 |
| Provider | 普通管理界面沿用「模型服务 / Model service」；具体账号或服务使用其展示名称。 |
| Route ID 与 Model Display Name | 调用字段使用「模型 ID / Model ID」，展示标签使用名称；不可把展示名当作路由身份。 |
| Provider Model 与 Route | 根据上下文区分服务提供的模型和供客户端调用的模型，不将所有实体全局替换成一个模糊术语。 |
| 联网搜索与内部网页来源 | 平台能力与内部 Web Access 来源分别解释，不为了统一措辞而合并不同领域概念。 |
| 标题和按钮 | 先写用户任务、动作与结果；协议和调度机制放在需要它们的配置表面解释。 |

中英文必须表达同一意图与承诺。例如 `Copied / 已复制` 只确认剪贴板操作，`Configuration written / 配置已写入` 只确认写入；不得把一侧翻译成连接成功。具体句子通过项目 i18n 维护，不以设计规范中的示例文字代替完整状态处理。

### 说明文案的保留标准

每段说明必须至少帮助用户完成一件事：选择、输入、理解结果或恢复错误。不能只因为技术事实正确就把它放进主界面。

| 内容 | 处理要求 |
| --- | --- |
| “内置 · 只读”、重复规则名称、重复检测目标 | 不作为默认装饰角标或辅助行；只有来源或可编辑性影响当前决策时才呈现。 |
| “不保存输入”、检测流程、记录存储与保留机制 | 不堆成测试页角标或说明折叠区。与当前操作无关的实现细节放在维护文档；必要的产品限制放在明确命名的详情入口。 |
| 文本会发送到哪里、是否联系模型服务 | 涉及敏感输入时，在输入操作附近用一句准确说明交代；不得用删减文案掩盖数据流向。 |
| 加载失败、保存失败、观察不完整、测试失败 | 保留明确反馈与恢复动作；不能为了页面干净而删除，不能与“没有命中”合并。 |
| 后端字段、表达式与匹配范围 | 技术值保持原样，外围解释使用用户可理解的语言。单位、条件与边界必须核对真实契约，不能仅凭字段名推断。 |

移除说明区域时同步移除无调用的 i18n key；中英文一起调整。设计示例不是静态模板，不应生成无实际内容的占位区。

## Colors

调色板以接近中性的冷灰为底，以钢蓝作为唯一主交互色。浅色与深色主题必须保持相同语义，不以简单反相替代逐项 token。

- **Background / Surface。** `background` 是工作台底色，`surface` 与 `popover` 是表格、请求路径及浮层表面。普通内容区优先直接落在背景上，用分隔线组织，不把每个 section 包进卡片。
- **Foreground / Muted。** `foreground` 用于标题、正文和关键数值；`muted-foreground` 用于解释、时间、辅助标签。不得用 muted 色承载必要操作或唯一错误信息。
- **Primary。** `primary` 标记主动作、当前路径、焦点和关键链接。它应像仪表上的选中信号，不应铺满大面积背景，也不应与 destructive 竞争同一层级。
- **Secondary / Accent。** `secondary` 用于次级动作；`accent` 用于当前导航、hover 和低强度选中态。hover 只改变色调，不制造高度跳变。
- **Semantic states。** `success` 只表示已确认健康或成功；`warning` 表示需要注意但尚可继续；`destructive` 表示错误、不可逆动作或失败。未知、未加载和“暂无数据”保持 neutral，不伪装成成功或失败。
- **Signal。** `signal` 是短暂确认色，例如技术值复制成功；不得取代 success 的持久状态语义。
- **Borders / Inputs。** `border` 建立区段、行与表面的层级；`input` 比普通边线略清晰，保证控件边界在两种主题下可辨。
- **Charts。** 正常序列按 `chart-1` 至 `chart-4` 使用同一钢蓝色阶；错误序列固定使用 `chart-5`。图表不使用彩虹配色，不为每个类别引入新的品牌色。

状态不能只靠颜色传达：healthy 使用圆点，warning 使用短横条，error 使用菱形，并同时提供文本。正文与背景、控件文字与控件表面必须达到 WCAG AA；正常字号文本目标对比度不低于 4.5:1。新增颜色必须同时定义浅色和深色值，并通过语义 token 使用，不能在 route 内写孤立色值。

## Typography

字体系统分为三种职责，数量越少越能维持仪表感。

1. **正文：IBM Plex Sans。** 英文正文、导航、标签和按钮使用 IBM Plex Sans，正文基准为 14px / 1.5。只使用 400、500、600 三个字重：400 阅读，500 操作与局部强调，600 标题。
2. **结构：IBM Plex Sans Condensed。** 英文页标题、section 标题、眉题和导航分组使用 Condensed 600。它建立控制面板式的纵向节奏，不用于长段正文。页标题在窄屏为 26px，`sm` 起为 30px；section 标题为 18px。
3. **技术：IBM Plex Mono。** Model ID、Route ID、URL、协议值、掩码密钥、代码、时间、计数和延迟使用等宽字。数字开启 tabular numerals，便于按列扫描；普通产品名称和解释文案不要等宽化。

中文界面使用 Noto Sans SC Variable；正文行高提高到 1.58。`.font-structural` 在 `zh-CN` 下也切换为 Noto Sans SC 并取消字距，避免拉丁窄体与中文强行混排。英文眉题可使用 0.14em 字距和 uppercase；中文眉题不增加字距，也不做伪大写。

文字层级：

- 眉题先说明信息域，例如 Setup、Monitor、System；它不能代替清晰页标题。
- 页标题说明当前任务，描述限制在约三行并优先写“做什么、得到什么”。
- section 标题描述一个可操作或可理解的子任务；描述文本补充影响与恢复方式。
- 表头、badge 和辅助标签保持短。ID 是操作必需信息时，提供完整值查看或复制；仅用于精确检索时可进入搜索字段，不必常驻显示。
- 标题使用 balanced wrapping，正文使用 pretty wrapping。禁止用字号过大制造空洞“英雄区”。

## Layout

整体是固定窗口壳层内的工作台，页面级滚动只有一个主要容器；表格、编辑器和 Sheet 可按任务需要拥有边界明确的局部视口，不能层层嵌套同方向滚动。

- 根壳层使用 `100svh`；顶部 titlebar 固定 40px，承载品牌、导航开关、breadcrumb 和桌面窗口控制。
- `md`（768px）及以上显示侧栏。展开宽度 256px，折叠宽度 48px；折叠状态保存在本机，`Ctrl/Cmd+B` 切换。折叠模式仍保留 40×40px 导航命中区与 tooltip。
- 主内容是唯一页面滚动容器：16px 内边距，桌面时右侧与底部保留 8px 壳层沟槽并使用 8px 圆角。内容居中，最大宽度 1800px。
- 常规 route 使用纵向 flex 与 24px 间距。Page Header 的标题块与动作在桌面两端对齐；小于 768px 时纵向堆叠，动作区域占满宽度。
- section 不默认使用卡片，而以 1px 顶边线、16px 顶内边距建立节奏；section header 与正文间距 14px，标题和动作间距 16px。
- 长设置表单默认限制在 64rem，保持阅读焦点；复杂 Model editor 可扩至 90rem。以表格、记录或输入/结果对照为主的高级能力页面应使用工作台可用宽度，不因导航归属“高级功能”而套用表单限宽。
- 表单宽度按内容语义选择：number 7rem、datetime 18rem、name 24rem、select 28rem、fill 36rem。这些值是宽布局 control column 的规范宽度；窄布局的 Field 与控件使用 `width: 100%`，随父容器自然收缩，不能横向溢出。
- Field 的重排依据 **Field 自身可用宽度**，不是只看 viewport。手机、窄 Sheet、侧边栏或窄列即使位于桌面窗口中，也必须采用上下布局；Field 自身足够宽时才切换为左右布局。
- 同一 FieldGroup 的宽布局共享一条 control column：左侧 label 区，中间可伸缩留白，右侧 Input / Select 区。上下同构的控件必须共用左右边界与宽度，不能因各行文案长度不同而错位。
- 监控与 Overview 的分析区在 1280px 起使用 12 列组合；Connect 在 1100px 起使用 5/7 分栏。较窄时按阅读顺序单列堆叠。
- 窄屏优先将多列管理表格重组为 `route-mobile-list`：每行使用“主体 + 操作”两列，次要字段进入 `dl` 或辅助文本。只有少量列且仍可阅读、操作的规则表可以保留表格；确实需要二维比较时允许局部横向滚动，不得让整页溢出。不以 768px 断点一刀切地隐藏所有表格。
- Request Spine 在桌面为三等分阶段，在移动端变为竖向流程；保留箭头与当前路径表达。编号若存在只表示请求流向，不表示配置步骤；数量按其真实统计口径命名，不强制称为可用数量。
- 指标条在桌面自动适配最小 9rem 列宽，在移动端固定两列，并补齐行间分隔线。
- Sheet 在移动端占满可用宽度；桌面普通、medium、Provider 编辑器宽度分别上限 32rem、40rem、56rem。body 独立滚动，footer 固定在底部并考虑 safe-area。
- 最小支持视口宽度为 320px。不得用固定像素定位绕过重排规则。

间距以 4px 为基础：4px 用于紧密关联，8px 用于控件内部与操作组，12px 用于并列小组件，16px 用于容器内边距，24px 用于 route 与主要区段，32px 仅用于较大章节或登录布局。优先复用现有 `gap` 与 Field 尺寸，不创建相邻但不同的新节奏。

## Elevation & Depth

Stravia 主要是平面界面。层级首先由背景色差、1px 边线、留白和排版建立，阴影只用于真正脱离文档流的表面。

- **零层：** 页面背景与普通 route section。无阴影，不为每段内容添加容器。
- **一层：** Request Spine、目标编辑 article、代码平面和必要的信息块。以 surface、border 和 8–12px 圆角区分；通常无阴影。
- **浮层：** Sheet、Dialog、AlertDialog、Dropdown 与 Popover 使用 `shadow-lg`。浅色为 `0 18px 48px rgb(17 20 23 / 0.16), 0 4px 12px rgb(17 20 23 / 0.1)`；深色为 `0 20px 56px rgb(0 0 0 / 0.44), 0 4px 14px rgb(0 0 0 / 0.28)`。
- **Tooltip：** 使用 foreground 反色表面和小箭头；体量小，不追加大阴影。
- **桌面材质：** Tauri 支持时，titlebar 与 shell 可使用系统半透明材质；Web 环境保持实色。半透明是宿主能力，不得在页面卡片上复制玻璃拟态。

hover、focus 或 pressed 不通过加大阴影“抬起”普通按钮。交互反馈应来自色调、焦点环与轻微按压缩放。不得把 `shadow-xl` / `shadow-2xl` 当作普通卡片样式。

## Shapes

形状语言是紧凑、机械、略带柔和，不尖锐也不玩具化。

- 4px：微型标签、特殊内部标记。
- 6px：紧凑导航项、tooltip、Provider mark。
- 8px：按钮、输入、选择器、主内容工作台和多数交互控件。
- 12px：Request Spine、空状态及需要被识别为完整模块的较大容器。
- full：状态圆点、进度条和真正的圆形控件；不要把普通按钮、badge 或卡片全面胶囊化。

Lucide 图标使用 16px 为常规尺寸，跟随文字颜色；图标只辅助识别，不能代替按钮名称或 aria-label。Provider mark 固定 28×28px：优先本地图标或安全来源图标；缺失时使用主色淡底与首字母，不出现纯黑占位块。

状态形状是语义的一部分：healthy 圆点、warning 横条、error 菱形。流程箭头、顶部规则线和矩形工作区共同维持“配线面板”特征；不要加入随意的 blob、波浪分隔、拟物旋钮或装饰圆环。

## Components

### App Shell 与导航

- 主导航按 Setup、Advanced Features、Monitor、System 分组；信息架构来自用户任务，不按后端 crate 或数据库表分组。
- 当前项使用 sidebar accent 与 `aria-current="page"`；hover 使用相同色系的低强度反馈。折叠时只隐藏文字，不移除状态、焦点或可访问名称。
- 移动导航使用左侧 Sheet，选中链接后关闭并恢复焦点。breadcrumb 只呈现当前层级，不复制第二套侧栏。
- titlebar 可拖动区域与窗口按钮必须共存；交互控件不能意外触发窗口拖动。

### Page Header、Section 与 Request Spine

- 每个主页面以 `PageHeader` 开始：眉题、唯一 `h1`、简洁描述，以及可选 actions / meta。新建等页面级动作放在 header；只影响局部开关或表单的保存动作靠近对应控件，不为统一位置而拉远，也不在多处复制。
- section 使用带 `aria-labelledby` 的 `h2`。说明写清影响和恢复；错误 section 提供 Retry，不只显示后端错误串。
- Request Spine 是 Overview 的请求路径组件，顺序为 API Key → Model → Provider。阶段可点击，数量说明其真实统计口径。配置不足时由独立的轻量任务卡给出当前推荐操作，请求路径不代替配置引导；无数据不显示无意义图表。

### Buttons 与操作优先级

- 默认按钮是每个局部任务的主动作；outline 是次级动作；ghost 用于行操作、工具栏和低权重动作；destructive 只用于不可逆确认。
- 默认高度 40px，large 44px，图标按钮可见区域与命中区至少 40×40px。按钮文本使用动词并说明结果，例如“保存代理设置”“连接首个模型服务”。
- hover 改变背景/前景；全局 focus-visible 提供 2px outline 与 2px offset，Button 再增加 3px 半透明 ring 和 ring 色边框；普通非 popup 按钮的 pressed 使用 `scale(0.96)`。常规过渡为 140ms、`cubic-bezier(0.2, 0, 0, 1)`，不使用弹跳或 overshoot。
- loading 保留按钮宽度，显示 Spinner 并使用 `aria-busy`；disabled 降低不透明度且不可交互，但不能替代校验错误说明。
- destructive action 必须在 AlertDialog 中写明对象名称与即时影响；确认按钮使用实心 destructive，不以普通 primary 假装危险动作。

### Fields 与配置表单

- 统一使用 Field、Label、Description、Error 结构。主 label 始终可见，并通过 `for` / `id` 与 Input 或 Select 关联；副 label 是补充解释、示例或影响说明，不得代替主 label。提交所必需的约束和校验错误必须保持可见，不能只藏在副 label 中。
- 设置页的主题与界面语言使用同宽 Select，不使用分段按钮；它们与同组 Input / Select 共用 control column。
- **窄容器：** 主 label 在上，Input / Select 在下，控件占满可用宽度，二者保持 8px 间距。存在副 label 时，不再另占一行；在主 label 右侧显示 16px 问号图标，使用 40×40px 命中区，在 hover、键盘 focus 或点击 / 触摸时显示完整说明。Tooltip 内容通过 `aria-describedby` 与触发器关联，不能只支持鼠标 hover。
- **宽容器：** Field 使用左右布局。左侧是 label 区：主 label 在上，副 label 在下；右侧是 Input / Select。两侧之间允许可伸缩留白，控件保持语义宽度，不为填满页面而无限拉伸。
- 宽布局中若没有副 label，主 label 相对 Input / Select 垂直居中；有副 label时，主副 label 作为一个文本栈整体与控件顶部对齐。
- 同一 FieldGroup 内连续出现相同输入结构时，所有 Input / Select 必须放入共享 control column，起点、终点和宽度一致。对齐由父级 grid / container 统一决定，不允许每一行根据 label 文案单独计算。
- 校验错误和与当前值直接相关的反馈放在控件下方：窄布局占整行，宽布局留在右侧 control column；不得破坏相邻控件的列对齐。
- 水平 field 继续用于 switch / checkbox 与短标签；输入密集区使用 FieldGroup。label、description、validation 必须与控件语义关联。
- 技术输入使用等宽字体。Secret 默认遮蔽，提供有名称的显示/隐藏按钮；提示必须符合实际查看、复制和编辑契约，不能未经依据声称「仅显示一次」。本规范不改变凭据展示或安全策略。
- Advanced 默认折叠，触发器提供 `aria-expanded` 与 `aria-controls`。只有在用户明确需要时显示协议级参数，不能让高级项淹没主流程。
- 保存失败保留用户输入并显示可行动错误；成功通过 toast 或局部 signal 确认。不要吞掉后端错误，也不要用自动重试掩盖配置问题。

### Tables、移动列表与技术值

- 管理数据列表优先使用 `$lib/components/ui/data-table`，基础语义结构复用 Table。排序、分页、过滤、列宽、固定表头与滚动行为在共享层维护，不在 route 中手写第二套表格或叠加互相覆盖的样式。
- 桌面表格用于对比多列实体；表头简短、数字右对齐、数值使用 tabular numerals。整行可点击时，内部按钮与菜单必须避免误触并保留独立名称和键盘操作。
- 列表中名称只承担一次主识别职责。辅助列提供真正不同的信息，例如匹配关键词或状态；不要在名称下重复 ID、目标和近义描述。ID、目标或描述可参与搜索，不要求全部可见。
- 规则详情使用关键词、匹配表达式、排除条件、路径限制、必需/可选组件等结构化内容。关联条件应显示可识别名称；高级参数按需折叠，不用原始 JSON 充当正常详情界面。不能为了“易读”改变表达式、距离单位或匹配语义。
- 搜索、排序、分页适合大量规则的查找与浏览；搜索后保持有效页码。分页限制数据量，局部滚动控制工作区高度，两者可共存。不得靠压缩行距塞入所有数据。
- 移动端按 Layout 的内容适配规则选择列表或简表，保留搜索、选择、状态与操作；不机械缩小桌面布局。
- `TechnicalValue` 负责截断、完整值 tooltip 和可选复制。复制成功使用短暂 signal 反馈；值本身不能因复制状态改变。
- Badge 用于协议、能力、有限状态与有意义的数量；关键词可使用轻量标签帮助扫描。不把所有元数据都变成 badge，数量较多时优先用文本、列表或详情浮层。

#### 表头视觉与密度

- 采用主流数据表的克制处理，而不是复制 React 等框架的实现。可参考 [shadcn/ui Data Table](https://ui.shadcn.com/docs/components/data-table) 的 ghost 排序按钮；继续使用本项目的 Svelte 组件、字体和语义 token。
- 表头是辅助扫描层，不是页面标题或厚重工具栏。共享表头字号为 `0.8rem`（默认根字号下约 13px）、字重 500；未排序标题使用 `muted-foreground`，当前排序标题使用 `foreground`。
- 单行表头的紧凑、默认、宽松密度基准分别为 40、44、48px，不含边线造成的亚像素差异。这是布局基准，不是截断内容的硬上限；分组、筛选行和文本缩放必须自然容纳。
- 数据行密度独立选择。规则浏览可以使用宽松数据行，同时保持紧凑表头；不得在 40px 按钮外再叠加大量垂直 padding，把表头撑成约 65px 的色块。
- 表头背景必须不透明，浅色和深色使用同一语义派生方式：当前共享值为 `color-mix(in oklab, var(--muted) 45%, var(--background))`。固定列的表头使用相同背景，滚动时不能透出下方数据；不为页面新增孤立灰色或主题特例。
- 底部使用单条细分隔线，保留与数据区的边界；不使用渐变、重阴影、双边线或每列一个按钮外框。
- 可排序与不可排序标题应在同一文本基线上，并与对应单元格内容起点对齐。排序按钮的内边距与外侧补偿共同计算，不能因复用通用按钮而把文字推离列边界。
- 未排序图标保持低权重但可发现，当前基准不透明度为 40%；hover 与键盘 focus-visible 提高至 100%。升序/降序使用清晰方向图标并保留 `aria-sort` 与下一步操作名称，不能只靠颜色区分。
- 排序按钮保持至少 40px 命中高度和可见焦点。hover、focus 与排序切换不得改变列宽、表头高度或挤动相邻标题。

#### 固定表头与滚动条

- 必须区分“表头是否固定”和“滚动条是否侵入表头”这两个问题，先确认实际现象。已经正确的吸顶行为不得因视觉调整而退化。
- 有限高且启用固定表头的表格，竖向滚动条的轨道和滑块从表头下方开始，不占表头区域。工具栏和分页器留在数据视口之外。
- 偏移以完整表头的实测高度为准，包括分组表头、控制列和行内筛选；不能按密度常量乘行数猜测。内容、字体或容器变化后必须保持对齐。
- 保留单一语义表格和统一的数据滚动视口，横向滚动时表头与数据列同步。不得复制两张表分别模拟表头和表体，也不得让内外两个滚动容器竞争滚轮与固定定位。
- 自定义滚动条复用既有 Bits UI ScrollArea 能力；同时保留滚轮、触控、键盘、滑块拖动和两端可达性。不能只靠 `::-webkit-scrollbar` 偏移来满足跨浏览器行为。
- 不调整没有启用固定表头的表格契约。修改共享滚动层时，必须同时检查普通表格、虚拟列表、横向滚动、固定列及多行表头。

### Tabs 与局部任务导航

- 同级工作区默认复用 shadcn-svelte Tabs 的分段样式，左对齐、按内容宽度占位，不拉成充满整页的下划线导航。页签内可用 Badge 显示相关数量，不把数值拼成另一条常驻状态说明。
- 真正改变 URL 的详情导航保留链接、`aria-current`、浏览器历史与在新标签页打开的能力；复用共享 Tabs 的视觉样式，不为统一外观改成没有链接语义的按钮，也不复制一套选中态样式。
- List 必须容纳 Trigger 的实际高度与焦点环。允许换行的 List 使用自适应高度，Trigger 保持独立的 40px 高度；不得用百分比高度使多行触发器撑出容器。
- 在 320px、500px 与桌面宽度下检查中文和英文标签：不得出现页签内部滚动条、裁切文字或重叠命中区；不能通过隐藏溢出来掩盖尺寸错误。
- 保留 Tabs 原有键盘导航、选中状态和面板关联。切换状态与视觉反馈一致，不为动效延迟内容操作。

### Metrics、Charts 与状态

- Metric Strip 只展示可解释的汇总。无流量时错误率显示 neutral 的“—”，不能显示红色 0% 或凭空推断健康。
- 图表只在数据存在时出现；无数据用带边线的短说明和下一步动作替代。Loading 使用与最终几何相近的 Skeleton，避免布局跳变。
- 状态标签同时包含形状、颜色和文字，并使用 `role="status"` 或与当前 section 的可访问语义关联。自动刷新频率作为辅助文本，不伪装成实时保证。

### Empty、Error 与 Loading

- Empty state 说明缺失依赖和最短恢复路径，例如先连接 Provider、再添加 Model、再创建 API Key。它应是紧凑、居中的任务提示，不是插画舞台。
- Error state 显示本地化后的真实错误，并提供 Retry 或返回安全状态的动作。部分数据失败时，保留仍可用数据并明确哪些内容未刷新。
- Skeleton 数量和网格接近目标内容；不使用无限 spinner 占据整页。未知状态保持 neutral。

### Sheets、Dialogs、Menus 与固定操作区

- 简短资源创建或编辑复用既有右侧 Sheet；Provider 详情与复杂 Model 编辑保留已有独立页面，不为统一外观迁入 Sheet。确认删除使用 AlertDialog；补充信息和短选择使用 Dialog、Popover 或 Dropdown。每种资源保持唯一编辑表面，不用同一种 modal 承担所有任务。
- Sheet header 固定表达任务，body 独立滚动，footer 靠底并保持按钮右对齐；移动端 footer 加 `safe-area-inset-bottom`。
- 页面级长编辑器的操作区可以 sticky，但必须在正常文档流中保留空间，不覆盖最后一个字段；取消在前、保存或创建在后。
- 浮层打开后管理焦点，关闭后回到触发器；Escape 与键盘导航遵循基础组件行为。

### Motion、触控与可访问性

- 动效快速、机械、可预测：140ms 用于控件与导航反馈，200ms 用于侧栏宽度；避免纯装饰动效、连续呼吸、视差、弹簧和大幅位移。
- `prefers-reduced-motion: reduce` 下动画和过渡降至 0.01ms、迭代一次，并关闭平滑滚动。
- 所有交互必须键盘可达并有清晰 focus-visible；图标按钮提供 aria-label；页面、导航、表格、状态和浮层使用原生语义优先。
- 交互命中区目标至少 40×40px。文本缩放、320px 宽度和中英文切换后不得遮挡关键操作。

### 内容与本地化

- 用户可见文本通过项目 i18n 消息提供；English 是默认与 fallback，中文表达必须保持同一意图和清晰度。
- 文案围绕目标、动作、结果和恢复。只有用户需要据此判断或恢复时，才暴露协议、存储或生命周期术语。
- 技术标识符、公共 API、协议字段、配置键与原始错误代码保持原文；解释文字本地化。
- 前端只展示和调用管理面，不复制后端业务规则。不得直接编辑生成的 `src/lib/paraglide/`。

## Do's and Don'ts

### Do

- 使用 `app.css` 的语义 token 和现有 UI primitive；新视觉语义先进入 token，再由组件消费。
- 保持“页标题 → 当前主任务 / 请求路径 → section → 数据或表单”的稳定扫描顺序；配置不足时，当前推荐操作优先于原理说明和空指标。
- 为每个 pending、empty、error、partial 和 success 状态定义可观察结果与恢复动作。
- 同时检查浅色、深色、中文、英文、桌面、移动和 Tauri 宿主差异。
- 把共同的表头、滚动、Tabs 或反馈问题修在共享层；页面仅选择任务所需布局、列与密度。
- 用边线、对齐、留白和字型建立层级；只让浮层产生明显阴影。
- 让技术值可扫描、可截断、可查看完整值，并在有价值时可复制。
- 保持 40px 交互命中区、可见键盘焦点、文本与形状双重状态编码。
- 在新增主页面时复用 `PageHeader`、`route-page`、`route-section`、Field 与现有 Empty / Status 模式。
- 在宽 FieldGroup 中统一 control column；在窄容器中把 label 与控件上下重排，并把副 label 收进可聚焦、可触摸的问号提示。

### Don't

- 不添加渐变主视觉、霓虹 glow、页面级玻璃卡片、3D 装饰、营销 hero 或大面积品牌插画。
- 不把每个 section 包进独立卡片，不用大圆角和重阴影补救信息架构。
- 不新增另一套蓝色、间距、圆角、按钮或表单模式；相邻但不一致的实现是缺陷，不是灵活性。
- 不用颜色作为唯一状态信号，不把 unknown / empty / zero 当作 success 或 error。
- 不用等宽字体排长段正文，不用 uppercase 和宽字距处理中文。
- 不隐藏必要 label，不把说明全塞进 placeholder，不让 tooltip 承载完成任务所必需的信息。
- 不用 viewport breakpoint 判断 Field 布局，不让窄 Sheet 或侧栏继承桌面左右排布；不允许相邻 Input / Select 因 label 长短而宽度错位。
- 不在移动端机械缩小桌面表格；按内容重组为列表、Sheet、分步内容或仍可操作的简表。
- 不用 bounce、overshoot、自动轮播、持续闪烁或无法关闭的装饰动画。
- 不在前端复制 Provider、Model、API Key 或协议转换业务规则；界面是管理面的适配层。
- 不新增强制接入向导、首次请求验证门槛、重复资源编辑表面或默认的逐客户端密钥创建流程。
- 不直接修改生成目录、绕过 i18n、泄露 secret，或在错误文案中暴露用户无法采取行动的内部细节。

## 设计变更的验收

设计完成以实际产品表面为准，不以“类名已改”“截图局部看起来合理”或静态检查通过代替。

1. **明确问题和边界。** 记录用户指出的现象、需要改变的部分、必须保留的行为。区分信息组织、视觉密度与运行故障，不把用户已经说明正常的行为当作待修缺陷。
2. **核对现有实现。** 先检查共享组件、调用页面、主题与 i18n。主流组件库仅提供处理方式参考，不引入第二套组件、依赖或设计 token。
3. **在真实页面操作。** 表头变更至少检查普通多列表格和长规则表；分别查看未排序、已排序、hover、键盘焦点、滚动后与内容为空时的表现。共享滚动层变化还需实际操作虚拟列表、分组/筛选表头、固定列和横向滚动。
4. **检查尺寸与主题。** 在浅色、深色、中英文、桌面和窄屏下检查。等主题、字体与过渡稳定后再截图，避免把中间帧误认成最终效果；表头底色、文字对齐、分页和最后一行都应可见且可操作。
5. **检查状态而非删文案后的空白。** 验证加载、未保存、失败、空结果、部分观察与恢复动作。精简介绍不得删除必要错误、敏感输入的数据流说明或改变已保存设置。
6. **运行相关验证。** UI 改动运行最直接相关的现有浏览器回归与 Svelte/静态检查；只有能防止真实行为回归时才保留新测试，不写仅断言源码、类名或固定文案的测试。文档修改核对引用、契约与自洽性，不运行无关构建。
7. **清理并交付。** 删除临时烟测页面、脚本和截图，关闭临时服务；同步无调用的文案清理与适用变更说明。用 Before / After 表逐项列出改变，报告实际执行的验证及未覆盖的浏览器或宿主，不把单一 Chromium 验证表述成全平台保证。
