---
version: alpha
name: Stravia 本地仪表控制台
description: 面向开发者与本地网关运维者的精密、克制、可观测管理界面。
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

> 本文件是 Stravia WebUI 的视觉与交互事实源，结构遵循 [Google Labs `design.md` 规范](https://github.com/google-labs-code/design.md/blob/main/docs/spec.md)。YAML token 给出规范值；正文解释这些值为何存在、何时使用，以及不得越过的边界。实现以 `frontend/stravia-webui/src/app.css` 和现有 UI 组件为准；二者发生偏差时必须在同一变更中校正。

## Overview

Stravia 是本地 AI 协议网关。管理界面的核心用户是配置 Provider、Model、API Key 与高级能力，并观察请求、延迟、错误和流量的开发者或本机管理员。界面必须帮助用户快速回答四个问题：**网关是否可用、请求会去哪里、当前配置缺什么、失败后下一步做什么**。

视觉参考不是营销型 SaaS 仪表盘，而是**桌边网络网关的控制面板与实验室仪表读数**：左侧像设备目录，主区域像规整的操作记录板，请求路径像配线面板，状态标记像小型信号灯。这个参考世界决定以下性格：

- **精密但不压迫。** 信息可以密集，结构必须平静；让边线、对齐和字体层级承担组织职责。
- **技术但不晦涩。** ID、URL、协议、时间和计数保持工程精度；说明文案围绕用户目标、动作与可观察结果。
- **本地且可信。** 不用夸张的品牌舞台、云端意象或“智能魔法”装饰；优先展示真实状态与恢复动作。
- **克制但不单调。** 低彩度钢蓝建立秩序；绿色、琥珀和红色仅在状态需要时出现。
- **桌面优先、移动可用。** 宽屏容纳表格、编辑器和分析图；窄屏重排为列表与抽屉，不缩小成不可操作的桌面截图。

当本文件没有规定具体样式时，按以下顺序决策：可读性与任务完成 > 状态可辨识性 > 与现有组件一致 > 信息密度 > 装饰性。Stravia 的视觉签名是**低彩度仪表底色上的 Request Spine（请求路径）**，而不是渐变、玻璃光效或大面积插画。

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
- 表头、badge 和辅助标签保持短；完整 ID 通过截断后的 tooltip 或复制能力提供。
- 标题使用 balanced wrapping，正文使用 pretty wrapping。禁止用字号过大制造空洞“英雄区”。

## Layout

整体是固定窗口壳层内的单一滚动工作台。

- 根壳层使用 `100svh`；顶部 titlebar 固定 40px，承载品牌、导航开关、breadcrumb 和桌面窗口控制。
- `md`（768px）及以上显示侧栏。展开宽度 256px，折叠宽度 48px；折叠状态保存在本机，`Ctrl/Cmd+B` 切换。折叠模式仍保留 40×40px 导航命中区与 tooltip。
- 主内容是唯一页面滚动容器：16px 内边距，桌面时右侧与底部保留 8px 壳层沟槽并使用 8px 圆角。内容居中，最大宽度 1800px。
- 常规 route 使用纵向 flex 与 24px 间距。Page Header 的标题块与动作在桌面两端对齐；小于 768px 时纵向堆叠，动作区域占满宽度。
- section 不默认使用卡片，而以 1px 顶边线、16px 顶内边距建立节奏；section header 与正文间距 14px，标题和动作间距 16px。
- 设置与高级能力页面限制在 64rem，保持长表单的阅读焦点；复杂 Model editor 可扩至 90rem，以容纳多目的地网格。
- 表单宽度按内容语义选择：number 7rem、datetime 18rem、name 24rem、select 28rem、fill 36rem。这些值是宽布局 control column 的规范宽度；窄布局的 Field 与控件使用 `width: 100%`，随父容器自然收缩，不能横向溢出。
- Field 的重排依据 **Field 自身可用宽度**，不是只看 viewport。手机、窄 Sheet、侧边栏或窄列即使位于桌面窗口中，也必须采用上下布局；Field 自身足够宽时才切换为左右布局。
- 同一 FieldGroup 的宽布局共享一条 control column：左侧 label 区，中间可伸缩留白，右侧 Input / Select 区。上下同构的控件必须共用左右边界与宽度，不能因各行文案长度不同而错位。
- 监控与 Overview 的分析区在 1280px 起使用 12 列组合；Connect 在 1100px 起使用 5/7 分栏。较窄时按阅读顺序单列堆叠。
- 768px 以下隐藏桌面表格，改为 `route-mobile-list`。每行使用“主体 + 操作”两列；次要字段进入 `dl` 或辅助文本，不靠横向滚动勉强保留所有列。
- Request Spine 在桌面为三等分阶段，在移动端变为竖向流程；箭头、编号、当前路径线和可用数量必须保留。
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

- 每个主页面以 `PageHeader` 开始：眉题、唯一 `h1`、简洁描述，以及可选 actions / meta。保存、新建等页面级动作放在 header，不散落在首个 section。
- section 使用带 `aria-labelledby` 的 `h2`。说明写清影响和恢复；错误 section 提供 Retry，不只显示后端错误串。
- Request Spine 是 Overview 的签名组件，顺序为 API Key → Model → Provider。阶段可点击并显示可用数量；空配置时直接给下一步动作，不显示无意义图表。

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
- 技术输入使用等宽字体。Secret 默认遮蔽，提供有名称的显示/隐藏按钮；新建 API Key 的 secret 只显示一次，并把“立即复制”作为当前任务。
- Advanced 默认折叠，触发器提供 `aria-expanded` 与 `aria-controls`。只有在用户明确需要时显示协议级参数，不能让高级项淹没主流程。
- 保存失败保留用户输入并显示可行动错误；成功通过 toast 或局部 signal 确认。不要吞掉后端错误，也不要用自动重试掩盖配置问题。

### Tables、移动列表与技术值

- 桌面表格用于对比多列实体；表头简短、数字右对齐、数值使用 tabular numerals。整行可点击时，内部按钮与菜单必须阻止误触并保留独立名称。
- 移动端把同一实体重组为主标签、辅助事实、状态与操作；不把整张表塞入横向滚动区。
- `TechnicalValue` 负责截断、完整值 tooltip 和可选复制。复制成功使用短暂 signal 反馈；值本身不能因复制状态改变。
- Badge 用于协议、能力和有限状态，不把所有元数据都变成 badge。数量较多时优先用文本、列表或详情浮层。

### Metrics、Charts 与状态

- Metric Strip 只展示可解释的汇总。无流量时错误率显示 neutral 的“—”，不能显示红色 0% 或凭空推断健康。
- 图表只在数据存在时出现；无数据用带边线的短说明和下一步动作替代。Loading 使用与最终几何相近的 Skeleton，避免布局跳变。
- 状态标签同时包含形状、颜色和文字，并使用 `role="status"` 或与当前 section 的可访问语义关联。自动刷新频率作为辅助文本，不伪装成实时保证。

### Empty、Error 与 Loading

- Empty state 说明缺失依赖和最短恢复路径，例如先连接 Provider、再添加 Model、再创建 API Key。它应是紧凑、居中的任务提示，不是插画舞台。
- Error state 显示本地化后的真实错误，并提供 Retry 或返回安全状态的动作。部分数据失败时，保留仍可用数据并明确哪些内容未刷新。
- Skeleton 数量和网格接近目标内容；不使用无限 spinner 占据整页。未知状态保持 neutral。

### Sheets、Dialogs、Menus 与固定操作区

- 创建/编辑实体使用右侧 Sheet；确认删除使用 AlertDialog；补充信息和短选择使用 Dialog、Popover 或 Dropdown。不要用同一种 modal 承担所有任务。
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
- 保持“页标题 → Request Spine / 主任务 → section → 数据或表单”的稳定扫描顺序。
- 为每个 pending、empty、error、partial 和 success 状态定义可观察结果与恢复动作。
- 同时检查浅色、深色、中文、英文、桌面、移动和 Tauri 宿主差异。
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
- 不在移动端缩小桌面表格；应重组为列表、Sheet 和分步内容。
- 不用 bounce、overshoot、自动轮播、持续闪烁或无法关闭的装饰动画。
- 不在前端复制 Provider、Model、API Key 或协议转换业务规则；界面是管理面的适配层。
- 不直接修改生成目录、绕过 i18n、泄露 secret，或在错误文案中暴露用户无法采取行动的内部细节。
