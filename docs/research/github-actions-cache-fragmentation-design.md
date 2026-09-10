# GitHub Actions cache 分区与写入设计

研究日期：2026-09-10。对象：`Stravia-AI/StraviaPlatform`。以下保留迁移前研究快照；实际实施策略见下一节，线上结果以对应 Actions run 为准。

## 实施策略

- CI 使用下文四类 Rust 分区及唯一 writer，失败不保存；release 的 Rust/Bun cache 关闭，Docker 不再使用 GHA cache。
- 可信 main push 可以保存缓存。另允许本仓库 `ci.yml` 在 main 上显式 `workflow_dispatch` 预热；普通手动 CI、PR、release caller 均不可写。该受限预热入口是原 push-only 方案的明确扩展。
- `cache_warm` 依次选择 `linux-test`、`linux-e2e`、`windows-debug`、`stable-registry`。每次仅运行对应 writer 的原有构建/测试路径，上一阶段成功后再启动下一阶段；不重复执行额外构建来填充 archive。
- Docker 使用 `.github/workflows/docker-cache.yml` 的可信 main producer，按架构写入 `ghcr.io/stravia-ai/straviaplatform:buildcache-amd64` 和 `:buildcache-arm64`，`mode=max`。release 仅恢复，不发布 cache；预热 workflow 不发布产品镜像或 Release。
- 首次迁移提交使用 `[skip ci]` 避免并发冷写入。确认旧运行结束、刷新 inventory 并按 ID 清理旧 Rust cache 后，顺序 dispatch 四个预热阶段，然后 dispatch 普通 CI 验证完整任务图。Docker cache 单独 dispatch。该标记只控制这次启动顺序，不代替验证。
- 容量数字仍是预算，不是硬限制或实测优化结果。GHCR cache 的存储成本、访问权限以及两架构真实构建结果需独立核验。

## 结论先行

1. 当前 8 条 cache 共 **9,162,311,070 bytes = 8.533067 GiB**；Rust 占 **99.1671%**。先收敛 Rust 写入者和重复分区，不值得先优化仅约 0.071 GiB 的 Bun/uv。
2. 推荐四类 Rust 热分区：**Linux pinned test、Linux pinned E2E mixed dev/release、Linux stable registry-only、Windows pinned shared debug**。每分区只有一个可信 main writer；其他 CI job 只恢复。`check` 不单独写编译缓存；release 编译缓存关闭。
3. **A 档优先：原生池预算 7.80 GiB（含 0.60 GiB 代际/增长空间），Docker 不写 `type=gha`，将来需要时迁入 registry。B 档有条件：Docker 两架构 `mode=min` 合计最多 0.60 GiB，原生池总预算 8.40 GiB。** B 档只有在实际仓库配额至少 10 GiB、两架构实测满足预算时才可启用；不是已证实可行的容量承诺。
4. `cache-shared-key` 替换默认 job 分区，**不是关闭 Rust/环境/依赖兼容性哈希**。本次运行解析到的 wrapper 使用 Swatinem v2.9.1；其源码在 `shared-key` 非空时忽略 `key`：不要同时配置两者后期待叠加。全部用途 discriminator 放进 shared key，不手工再 hash Cargo.lock/rustc。[S2][S3]
5. 分区名稳定不等于字节数有界。Rust immutable cache 遇到依赖、toolchain 或环境变化会产生新一代。必须同时控制**一个 writer、一个常驻代际、升级时预先释放空间**；不能等 GitHub LRU 代替容量管理。

## 1. 范围、计量与线上事实

本次以只读命令重新获取快照，结果与给定数字一致：

```sh
gh cache list --repo Stravia-AI/StraviaPlatform --limit 100 --json id,key,ref,sizeInBytes,createdAt,lastAccessedAt --jq '.[]'
```

以下全部条目位于 `refs/heads/main`。`GiB = bytes / 1,073,741,824`；占比以 9,162,311,070 bytes 为分母。

| ID | key / 类别 | bytes | GiB | 总量占比 |
|---|---|---:|---:|---:|
| 7524330050 | `v0-rust-postgres-e2e-Linux-x64-6750113f-eb7414bd` | 1,772,956,582 | 1.651194 | 19.3505% |
| 7523948702 | `v0-rust-backend-e2e-Linux-x64-6750113f-eb7414bd` | 1,837,906,287 | 1.711684 | 20.0594% |
| 7523545144 | `v0-rust-web-access-browser-Windows_NT-x64-cab9879f-156c5bc4` | 1,917,426,905 | 1.785743 | 20.9273% |
| 7523330408 | `v0-rust-unit-tests-Linux-x64-095333cd-eb7414bd` | 1,786,598,170 | 1.663899 | 19.4994% |
| 7523283390 | `v0-rust-unit-tests-Linux-x64-a7ae7303-eb7414bd` | 1,771,114,318 | 1.649479 | 19.3304% |
| 7119183909 | `bun-tw6RmwqZSA0Bnj0yIUzvSupv0Nk=` | 35,658,635 | 0.033210 | 0.3892% |
| 7523119128 | `bun-HkdUWsJeWe4WiHi5oTwlgt+dgOI=` | 39,083,483 | 0.036399 | 0.4266% |
| 7523937620 | `setup-uv-2-x86_64-unknown-linux-gnu-ubuntu-24.04-3.12.3-94865497f97743a6772806831c9bcece1350e675d21a66c9a326c3c99593669d` | 1,566,690 | 0.001459 | 0.0171% |
| 合计 | 8 条 | **9,162,311,070** | **8.533067** | **100%** |

五条 Rust 合计 9,086,002,262 bytes = 8.461999 GiB；Bun 两条共 74,742,118 bytes = 0.069609 GiB；uv 0.001459 GiB。

- **事实**：后续对应 job 日志确认 `095333cd` 是 pinned unit、`a7ae7303` 是 stable unit；单凭 cache list 中不透明 hash 不能做这种归类。Bun 两条也不能仅凭 hash 认定 OS。
- **事实**：当前快照没有 `check`、`desktop-smoke`、release Rust 或 Docker cache。**仅凭缺失不能断言 LRU 淘汰、没运行、构建失败或没保存。** 本次另取得下述成功运行和 desktop 保存日志，能加强 desktop 的淘汰判断，但不能为其他缺失条目代替证据。
- **线上补充证据**：[main push run 34411850022](https://github.com/Stravia-AI/StraviaPlatform/actions/runs/34411850022) 成功；2026-09-09 UTC 的 job 完成顺序为 check 22:39、desktop-smoke 22:58、stable 23:04、pinned 23:06、web-access 23:15、backend proxy 23:31、sqlite 23:37、postgres 23:46、admin 23:53。unit cache 创建时间与这两个 toolchain job 对应，支持 7523330408 为 pinned、7523283390 为 stable 的给定归类。backend cache 创建时间对应 proxy，后完成的 sqlite/admin 未产生另一个同 key 条目，支持 first-writer-wins 推断，而不是 admin 的 mixed profile 已完整保存。
- **更强但仍有限的因果证据**：[desktop-smoke job 102672464559](https://github.com/Stravia-AI/StraviaPlatform/actions/runs/34411850022/job/102672464559) 的 22:58 Rust post-action 日志为 `Cache up-to-date.`：当时已命中既有 desktop cache、没有新上传；之后清单已无 desktop key。这证明一条刚使用的 desktop cache 随后消失。结合晚完成的五条约 1.65–1.79 GiB Rust archive 和 LRU 规则，**高度可信地推断容量淘汰导致热 desktop cache 消失**；未读取手工删除审计，不能把 LRU 因果写成完全证实。check 尚无对应保存日志；Docker/release 缺失仍只作为快照事实。
- **事实**：除较早创建的 Bun ID 7119183909 外，本批条目创建于 2026-09-09 22:58–23:46 UTC；单个时间切片不能给出命中率或生命周期。
- **余量**：按题设 GitHub 界面的二进制容量口径，即 `10 × 2^30 bytes`，剩 **1,575,107,170 bytes = 1.466933 GiB**，已经小于 1.5 GiB 目标；无法再容纳一条现有大小的 Rust cache 而不触及限额。
- **单位安全边界**：官方文字写“10 GB”，并说明这是默认限额、现可付费调高，不代表所有仓库的硬上限。[S1] 若保守解释为 `10,000,000,000 bytes`，余量只有 **837,688,930 bytes = 0.780159 GiB**。实施前记录仓库实际设置及 byte 限额，不把 GB/GiB 混为一谈；本设计不通过付费扩容解决问题。A 档 7.80 GiB 在十进制 10 GB 下仍留约 1.513 GiB，B 档则不满足该保守口径。

## 2. 当前生成规则与诊断

### 2.1 Rust：重复 archive，而非一个共享 Cargo 仓库

本地所有 Rust setup 使用 `actions-rust-lang/setup-rust-toolchain@v1`，只设置 toolchain（check 另装 rustfmt），未关闭默认 cache。wrapper 默认 `cache: true`、`cache-targets: true`、`cache-save-if: true`、`cache-on-failure: true`，并委托 Swatinem/rust-cache；注意 wrapper 的失败保存默认值与 Swatinem README 的直接使用默认值不同。[S2]

本次运行解析到的 v2.9.1 逻辑可简写为：

```text
v0-rust-
  (shared-key，若非空；否则 [key-]job_id)
  -OS-arch
  -hash(rustc versions + CARGO/CC/CFLAGS/CXX/CMAKE/RUST* 环境)
  -hash(依赖 manifests/lockfiles/toolchain/config)
```

OS/arch、实际编译器与环境、依赖 hash 已内建；restore prefix 保留环境边界、允许依赖变化时回退。[S3] 同一 shared key 不保证同一最终 key：已安装的 toolchain、环境变量、路径/压缩形成的 cache version 也要一致。**本次运行日志已解释 unit/E2E 的 hash 差异**：pinned unit 的 `095333cd` 只列 Rust 1.98.1，runner image 为 `20260907.300.1`；backend proxy 的 `6750113f` 同时列预装 1.98.0 和新装 1.98.1，image 为 `20260831.293.1`。实际运行解析到 `setup-rust-toolchain` SHA `166cdcfd…`，它固定调用 `rust-cache` SHA `c1937114…`（v2.9.1）。该版本的 `getRustVersions` 遍历 `rustup toolchain list`，并使用 `Set<RustVersion>` 保存新建对象；值相同但对象不同的记录不会结构化去重，正好解释日志中的重复 1.98.1 及 pinned/stable 不同 hash。v2.9.2 已改为 `Set<string>` 去重，wrapper 将来升级时应预期一次环境代切换。[S2][S3] hosted runner image 分阶段 rollout 还会引入真正不同的预装版本；**shared-key 只解决 job-id 重复，不消除 runner/toolchain 集合碎片；不得关掉环境 hash 强制命中。** 以上来自 [run 34411850022](https://github.com/Stravia-AI/StraviaPlatform/actions/runs/34411850022) 的 Rust cache 配置日志；遍历规则见 [S3] config.ts。

默认缓存 registry/git、部分 cargo bin 与 target 中依赖；默认排除 workspace 自身产物、增量产物，保存前清理过期/不再使用依赖。[S3] 因此不能把 archive 当作可直接发布的 server/desktop 二进制，消费者仍要正常运行 Cargo。

| 现状 | 判断 |
|---|---|
| backend/postgres 相同环境与依赖 hash，job_id 不同，各约 1.7 GiB | **推断**：公共依赖高度重复，适合合并；快照不是文件级去重测量，不能声称能精准节省两条之和。 |
| backend matrix 三个 suite 共享 `backend-e2e` job_id | 相同环境时同 key 的并发 miss 会竞争保存；cache 不可原地合并，赢家内容决定以后恢复的覆盖范围。[S1][S3] |
| admin 同时 `build:server` release + `build:server:debug`，proxy dev server + devtools，sqlite/postgres dev server | **不能让随机赢家定义分区内容**。选自然覆盖 dev/release server 的 admin writer；proxy 独有的 devtools 依赖允许冷编译，不为填 cache 增加重复构建。mixed 预算未实测。 |
| unit pinned/stable | 实际 rustc hash 能隔离 ABI；stable 升级产生新代，完整 target 热 cache 性价比需与编译时间比较。推荐 stable 只缓存下载数据。 |
| browser 与 desktop-smoke 都 Windows x64、pinned，但用途不同 | 可以共享同一 host/CRT 下的 debug 依赖 archive；Cargo 仍按 features/profile 校验，并不让 test、desktop、release 产物相互冒充。选择耗时更长且已有 1.786 GiB 实测 archive 的 browser writer；desktop 差集冷编译。 |
| `check` 全 workspace cargo check 含 desktop，unit exclude desktop | 共享 pinned test 只恢复，可能无法覆盖 check 特有依赖；接受冷编译这部分，避免再留一条大 archive。 |

`Taskfile.yml`、`Cargo.toml`、`.cargo/config.toml` 是上述 profile/CRT 事实的本地证据。Windows 使用 `+crt-static`，Linux ubuntu-24.04 GNU 与 Docker bookworm、musl、ARM64 均不得混用 target archive。

### 2.2 Bun 与 uv

- `setup-bun@v2` 默认缓存的是 **Bun executable，不是 `node_modules`、Bun package download cache 或 Playwright Chromium**。key 是 `bun-` 加下载 URL 的 SHA-1/base64，URL 间接区分版本/OS/arch/CPU 变体。[S4] 本地没有另外配置 Bun 依赖 cache；`bun ci` 不等于已启用依赖缓存。
- 多 job 同版本/平台会竞争同一个 executable key。体积小，但仍应明确 writer。该 action 有 `no-cache`，**没有独立 save-if/restore-only 输入**。最小且正确的方案是只在指定可信 writer 启用内置 cache，其他 job（含 PR/release）`no-cache: true`，直接下载；不伪造 `save-if` 输入。
- `setup-uv@v10.0.1` key 内建 cache schema、arch/platform/OS version、Python version、依赖 glob hash、pruned/python 标记与可选 suffix，不依赖 job_id。[S5] backend 三个 suite 与 postgres 使用相同 uv 环境时共享 key；指定 admin 单 writer，其他只恢复即可。
- uv v10 的 `enable-cache: auto` 在 tag push/release 等事件默认关闭。因此不能笼统断言当前 release 调用 CI 一定创建 uv tag cache。建议显式 `enable-cache: true` + `save-cache: false` 让 release 恢复 main 的 uv cache，`cache-python: false` 防止新增完整 Python 安装缓存。[S5]

### 2.3 Release 与 Docker

`release.yml` 的 `checks` 调用整个 `ci.yml`，`github` context 关联 caller，不会因进入 reusable workflow 就变成 main push。[S6] tag release 的 CI 子 job 若缓存写入未约束，可以创建 tag 范围 Rust/Bun cache。不同 tag 无法互读，main 也不能读取 tag cache；一次发布只复用本 tag 重跑，而 main cache 可以供 tag 恢复。[S1]

`build-server` 六行矩阵（Linux x64/ARM64 × GNU/musl、Windows x64/ARM64），`build-desktop` 四行（Linux/Windows × x64/ARM64）均默认写 Rust cache。OS/arch 会自动隔离，但同 host 的 GNU/musl 默认 job key 不能表达完整用途，而且 musl target、交叉工具设置部分发生于 Rust setup 之后。即便 Cargo 自己隔离 target 路径，也不应让这些产物竞争一个 archive。**推荐这些发布构建全部 `cache: false`，不另养十组低频 release target cache。**

Docker 当前两个 job：`docker-build` 与 `publish-image`，各自 amd64/arm64 matrix，`cache-from/to: type=gha,scope=docker-{amd64,arm64}`，export `mode=max`。同架构的 validation/publish 是两个 exporter；当前依赖图先 validation 后 publication，通常是重复写而非并发竞争，但不同运行仍需隔离 writer。`scope` 是命名空间，不是独立配额：**两架构 Docker 与 Rust 共用仓库 Actions cache 池**。[S7]

Dockerfile 多阶段 builder 含 Rust、Go、系统开发包与 Web 构建；`max` 导出中间阶段层，不能把它视为只有最终 runtime image 大小。当前 Docker cache 为零不代表以后不会冲掉 Rust。并且 `RUN --mount=type=cache` 中的 Cargo target/registry 与 Bun mount **默认不会随 GHA layer exporter 跨 runner 保存**；没有 cache-dance 的本仓库不能声称 `mode=max` 已把 mounted target 作为 Rust 增量 cache 持久化。[S8] 整条 RUN layer 命中与 mount 内容复用是两回事。

## 3. 目标分区、writer 与容量

以下预算全是**设计上限/假设，不是优化后实测**。同名分区允许 dev/test/release 目录共存，但不改变 Cargo 的 ABI/profile 判定。`pinned` 是逻辑角色，实际版本由 action 的 rustc hash 管理，不把版本再写入 key。

| 分区 shared-key / 类别 | 兼容范围与覆盖 | 唯一 writer（可信 main CI push） | restore-only consumers | 常驻预算 GiB |
|---|---|---|---|---:|
| `ci-v1-linux-gnu-pinned-test` | Ubuntu 24.04 x64 GNU；pinned；test/dev 依赖 | `unit-tests` pinned | `check`、同一 unit PR/release 调用 | 1.90 |
| `ci-v1-linux-gnu-pinned-e2e-mixed` | Ubuntu 24.04 x64 GNU；pinned；dev/release server，允许 devtools 差集冷编译 | `backend-e2e` admin | backend proxy/sqlite、postgres、所有 E2E PR/release 调用 | 2.80 |
| `ci-v1-linux-gnu-stable-registry` | Ubuntu 24.04 x64；stable 实际 rustc；registry/git，**无 target/bin** | `unit-tests` stable | stable PR/release 调用 | 0.25 |
| `ci-v1-windows-msvc-pinned-debug` | Windows 2022 x64 MSVC static CRT；pinned；browser test 为主，允许 desktop 差集冷编译 | `web-access-browser` | `desktop-smoke`、browser PR/release 调用 | 2.10 |
| Bun executable Linux x64 / Windows x64 | 沿用 action URL key，不新增手写 hash | Linux `check`；Windows `desktop-smoke` | 最小方案其余关闭 cache，不是 restore-only | 0.10（两者合计） |
| uv E2E downloads | Linux x64 + Python + lock hash；不含 Python 安装 | `backend-e2e` admin | backend 其他 suite、postgres、PR/release 调用 | 0.05 |
| **原生有效代合计** | | | | **7.20** |
| 有限旧代/小缓存增长空间 | 不是每分区都可保留第二代 | 受控清理 | | **0.60** |
| **A 档池内上限** | Docker off 或 registry 外置 | | | **7.80** |
| B 档 Docker `mode=min` | 每架构 0.30；仅 main producer | 专门 main Docker warm job 每架构一行 | release validation/publish | **+0.60** |
| **B 档池内上限** | 仅配额与实测满足时采用 | | | **8.40** |

二进制 10 GiB 下 A 留 2.20 GiB、B 留 1.60 GiB；两档都不少于 1.5 GiB。**余量不是每次大 archive 替换都够**：E2E 新一代预算 2.80 GiB，因此即便 A 也不能在满预算时先上传整条再删旧代。

### 3.1 为什么不是一个 Linux shared target

把 pinned test 与 E2E 全合并会让 release profile 扩大每个 unit/check 的下载量，且需要扩大 writer 工作范围；保留两个用途分区更容易解释成本。stable 使用 registry-only 是以编译时间换约一整条 target 的空间，不承诺测试变快。Windows 只有一个共享 debug 分区；若 desktop+browser 合集实测超过 2.10 GiB，优先保留 browser writer 并让 desktop restore-only（接受 desktop 专属依赖每次编译），而不是自动新建 desktop 分区。

单 writer 只保存其自然执行路径产生的依赖。E2E 的 devtools 差集、Windows desktop 的 Tauri 差集允许由 consumer 冷编译；这比为了填满 cache 重复运行构建或测试更容易维护。不能声称随机多个 writer 能渐进合并 immutable cache 内容。

### 3.2 代际与硬容量控制

1. 同时只保留每个大分区一个有效代；0.60 GiB 只容纳小条目/短暂波动，不是三个 Rust 分区的滚动双份预算。
2. 依赖/toolchain/environment 更新前，把预计所有要新增的 archive 相加；若 `当前 bytes + 待上传 bytes` 会超过预算或实际限额，**在维护窗口先关闭其他 writer，删除受影响分区旧代，再顺序 warm 新代**。这会产生一次明确的 cold build，但不让 LRU 随机删除其他热分区。
3. 普通源码变更沿用依赖 key；不要添加 SHA、run_id、日期、suite 到大 cache key。shared key 不再细化每个 E2E suite。
4. main workflow 当前 `ci-${{ github.ref }}` concurrency 可限制同 ref 重叠，但仍保留单 writer 的 job/matrix gate，不能只依赖取消运行。新独立 Docker writer 也要有按架构的 concurrency；只有一个定义可写对应 scope/ref。
5. GitHub 没有在上述 action 输入里提供每分区字节硬上限。表中预算需由采样和受控清理落实，不声称配置 shared key 就自动稳定。大分区超预算时停写该分区、选择缩小覆盖或 registry-only；不要自动扩容越过总预算。

## 4. 每个现有 job 的配置迁移表

`W` 定义为：**本仓库、直接 CI workflow 的 main push**。具体表达式见下一节；其他事件一律只读/关闭，release workflow_dispatch 即使选择 main 也不能获得 writer 权。所有开启 Rust cache 的行统一 `cache-bin: false`、`cache-on-failure: false`、不设 `cache-key`，workspace 默认保持一致。

| workflow/job | 当前 Rust 来源 | 目标 `cache-shared-key` | `cache-targets` | `cache-save-if` | Bun / uv |
|---|---|---|---|---|---|
| CI/check | 默认 `check` job key；cargo check | Linux pinned test | `true` | `false` | Bun 仅 W 开缓存；无 uv |
| CI/unit-tests pinned | 默认 `unit-tests` + rustc hash | Linux pinned test | `true` | W 且 pinned matrix | Bun cache 关闭；无 uv |
| CI/unit-tests stable | 同 job key，不同 rustc hash | Linux stable registry | `false` | W 且 stable matrix | 无 Bun/uv |
| CI/web-access-browser | 默认同名 job key | Windows pinned debug | `true` | W | 无 Bun/uv |
| CI/backend-e2e proxy | 默认 backend 共用 key | Linux pinned E2E mixed | `true` | `false` | Bun 关闭；uv restore-only |
| CI/backend-e2e admin | 默认 backend 共用 key | Linux pinned E2E mixed | `true` | W 且 `suite == 'admin'` | Bun 关闭；uv 仅 W 保存 |
| CI/backend-e2e storage:sqlite | 默认 backend 共用 key | Linux pinned E2E mixed | `true` | `false` | Bun 关闭；uv restore-only |
| CI/postgres-e2e | 默认 postgres job key | Linux pinned E2E mixed | `true` | `false` | Bun 关闭；uv restore-only |
| CI/webui-e2e | 无 Rust | 不适用 | 不适用 | 不适用 | Bun 关闭；Chromium 无新增 cache |
| CI/desktop-smoke | 默认 desktop job key；debug Tauri | Windows pinned debug | `true` | `false` | Bun 仅 W 开缓存；无 uv |
| Release/checks（workflow_call） | 再次执行以上所有 CI jobs | 同上，沿用 main 分区 | 同上 | **全部 false** | Bun 关闭；uv 显式 restore-only |
| Release/build-server linux-x86_64-gnu | build-server/Linux/x64 + hashes | **关闭 Rust cache** | 不适用 | `false`（防御性） | Bun 关闭 |
| Release/build-server linux-x86_64-musl | 同 host job key，target 在 setup 后添加 | **关闭**；未来若启用须独立 musl/release 分区 | 不适用 | `false` | Bun 关闭 |
| Release/build-server linux-aarch64-gnu | build-server/Linux/arm64 + hashes | **关闭** | 不适用 | `false` | Bun 关闭 |
| Release/build-server linux-aarch64-musl | 同 host job key | **关闭**；未来独立 musl/release | 不适用 | `false` | Bun 关闭 |
| Release/build-server windows-x86_64 | build-server/Windows/x64 + hashes | **关闭**，不恢复 CI debug 大包 | 不适用 | `false` | Bun 关闭 |
| Release/build-server windows-aarch64 | build-server/Windows/arm64 + hashes | **关闭**；native ARM64 CMake 不混 x64 | 不适用 | `false` | Bun 关闭 |
| Release/build-desktop linux-x86_64、linux-aarch64 | build-desktop + OS/arch/hashes，两分区 | **均关闭** | 不适用 | `false` | 两行均关闭 Bun |
| Release/build-desktop windows-x86_64、windows-aarch64 | build-desktop + OS/arch/hashes，两分区 | **均关闭** | 不适用 | `false` | 两行均关闭 Bun |
| Release/docker-build amd64、arm64 | GHA `docker-amd64` / `docker-arm64`，max export | A：关闭或 registry read；B：新 `docker-v1-{arch}-min` read | 不适用 | **无 cache-to** | 不直接使用 setup Bun/uv/Rust；Dockerfile 内运行 |
| Release/publish-image amd64、arm64 | 相同两 GHA scope，再次 max export | 同上一行，只读 | 不适用 | **无 cache-to** | 同上 |
| Release/nix-build 两 system | 不调用上述 setup/cache action | 不新增原生池分区 | 不适用 | 不适用 | 不新增 Bun/uv cache |
| Release/prepare、collect-assets、prepare-draft、publish-image-manifest、publish-release | 无本设计涉及的 cache producer | 不新增 | 不适用 | 不适用 | artifact/发布文件不当作 Actions dependency cache |

表内 Linux pinned test / E2E mixed / stable registry / Windows pinned debug 分别指第 3 节完整 shared-key。release 构建选择关闭而非新设 registry-only，避免额外 arch/toolchain 分区；以后若性能证据支持，必须在现有总预算内重新取舍。

## 5. 最小 YAML 实施示例（未应用）

### 5.1 可信 writer gate 与 Rust

只检查 `github.ref` 不足以挡住 release workflow_dispatch/main；只检查 `event_name != pull_request` 也过宽。以下有意只允许 **CI 自身 main push**；手动 warm 可用可信 main 新提交触发，或后续显式设计受限 dispatch，而不是默认放开。

```yaml
# 放在 ci.yml 已有 env 中；用于 cache policy 的变量名不使用 RUST/CARGO 前缀。
env:
  CACHE_WRITE_ALLOWED: ${{ github.repository == 'Stravia-AI/StraviaPlatform' && github.event_name == 'push' && github.ref == 'refs/heads/main' && github.workflow_ref == 'Stravia-AI/StraviaPlatform/.github/workflows/ci.yml@refs/heads/main' }}

# unit-tests 的 Setup Rust；同一 action 管理 toolchain 和缓存。
- uses: actions-rust-lang/setup-rust-toolchain@v1
  with:
    toolchain: ${{ matrix.toolchain }}
    cache-shared-key: ${{ matrix.toolchain == 'stable' && 'ci-v1-linux-gnu-stable-registry' || 'ci-v1-linux-gnu-pinned-test' }}
    cache-targets: ${{ matrix.toolchain != 'stable' }}
    cache-save-if: ${{ env.CACHE_WRITE_ALLOWED == 'true' }}
    cache-on-failure: 'false'
    cache-bin: 'false'
```

这两行 matrix 各写不同分区，所以可以共用 W；其他 job 按表替换 shared-key。`CACHE_WRITE_ALLOWED` 要合并进已有 env，不能覆盖原有版本变量。GitHub context 的 `workflow_ref` 用于限定入口，实施时在普通 CI、PR、tag caller、release dispatch/main 各记录 policy 求值，确认 caller context。[S6][S9]

```yaml
# backend-e2e 的 Setup Rust：仅 admin 写同一分区。
- uses: actions-rust-lang/setup-rust-toolchain@v1
  with:
    toolchain: ${{ env.RUST_VERSION }}
    cache-shared-key: ci-v1-linux-gnu-pinned-e2e-mixed
    cache-targets: 'true'
    cache-save-if: ${{ env.CACHE_WRITE_ALLOWED == 'true' && matrix.suite == 'admin' }}
    cache-on-failure: 'false'
    cache-bin: 'false'

```

postgres 复制相同 shared-key，但 `cache-save-if: 'false'`。Windows `web-access-browser` 是 shared debug 分区唯一 writer；`desktop-smoke` 只恢复，允许其 Tauri 专属依赖冷编译。这样冷启动时消费者可能先 miss，下一次才能热；不要增加重复构建/测试只为填 cache，也不要在同一 key 上靠多个消费者补写。

release build-server/build-desktop：

```yaml
- uses: actions-rust-lang/setup-rust-toolchain@v1
  with:
    toolchain: ${{ env.RUST_VERSION }}
    cache: 'false'
    cache-save-if: 'false'
```

若未来必须直接使用 `Swatinem/rust-cache@v2`，先在 toolchain setup 设置 `cache: 'false'`，再使用 `shared-key`、`cache-targets`、`save-if` 等原生输入，不能开两个 cache action 重复操作同一 target。不要添加手工 Cargo.lock 或 rustc hash，也不要设置 `shared-key` 后再用 `key: ${{ matrix.suite }}` 尝试二次分区；在当前实现后者被忽略。[S3]

### 5.2 Bun 和 uv

```yaml
# 仅 CI/check（Linux）和 CI/desktop-smoke（Windows）使用此配置。
- uses: oven-sh/setup-bun@v2
  with:
    bun-version: ${{ env.BUN_VERSION }}
    no-cache: ${{ env.CACHE_WRITE_ALLOWED != 'true' }}

# 所有其他 Bun steps（含所有 release steps）保留版本，设置：
# no-cache: true

# backend matrix 的 uv；postgres 改 save-cache: 'false'。
- uses: astral-sh/setup-uv@v10.0.1
  with:
    version: ${{ env.UV_VERSION }}
    enable-cache: 'true'
    restore-cache: 'true'
    save-cache: ${{ env.CACHE_WRITE_ALLOWED == 'true' && matrix.suite == 'admin' }}
    cache-python: 'false'
```

不引入新的 node_modules、Playwright 或 `.venv` archive。uv 暂保留原 dependency glob 和 prune 默认值，避免无性能证据就改变缓存覆盖；实际 uv 稳定 key 如因 Python 版本不同分裂，统一 Python 选择或明确额外预算，不能忽略差异。

### 5.3 Docker A / B 两档

**A：立即可实施的安全基线是移除 release 的 `cache-to: type=gha...`；可以保留现有 cache-from 暂时只读或一并关闭。** 真正迁入 registry 需要后续明确授权和 main producer，当前 repo 的 Docker jobs 只在 release 中，不能宣称已有 main writer。

后续在可信 main push 的专用 producer（每架构一行、`packages: write`、Docker login、setup-buildx）中使用：

```yaml
# matrix.arch 为 amd64 或 arm64；现有 build-push-action 的其他构建参数不变。
with:
  cache-from: type=registry,ref=${{ env.IMAGE_NAME }}:buildcache-main-${{ matrix.arch }}
  cache-to: type=registry,ref=${{ env.IMAGE_NAME }}:buildcache-main-${{ matrix.arch }},mode=max
```

release `docker-build` / `publish-image` **仅保留 cache-from，不写 cache-to**。registry ref 不与发行 image tag 混用；registry 数据不占 Actions cache pool，但仍有 registry 存储/保留成本和读取权限风险。[S10] 本研究不创建该资源。若不新增 main producer，就接受冷 Docker 构建，不能改让 tag publication 写共享 main cache。registry 也不因此自动保存 cache mounts。

**B：不引入 registry 时的限额方案**，同样要求后续增加可信 main producer；两个架构各一个 writer，release 两 job 都只恢复：

```yaml
# 仅 main producer，不能放在现有 release/tag job 中开放写入。
with:
  cache-from: type=gha,version=2,scope=docker-v1-${{ matrix.arch }}-min
  cache-to: type=gha,version=2,scope=docker-v1-${{ matrix.arch }}-min,mode=min
```

`mode=min` 只导出最终 image 所需层，牺牲中间 builder 复用，**不是 0.30 GiB 的限额开关**。[S7][S10] 两架构全部 GHA entries（含 metadata/blobs/旧代）实测合计超过 0.60 GiB 或仓库实际不是至少 10 GiB 时，B 不通过验收，退回 A/off；不要用 `ignore-error` 掩盖超额。API 使用 v2，Docker 官方已于 2025-04-15 停用 v1。[S8]

## 6. 迁移、一次性清理、回滚

### 6.1 迁移顺序

1. 保存 cache inventory 与近期 main/PR/release 日志，记录当前 runner image、resolved action SHA、rustc/environment hash、cache bytes/上传时间和编译时间。来源网页主分支与浮动 action tag 会变化，应把本次实际执行版本写进后续实施记录。
2. **先落写入安全 gate**：CI 单 writer，Rust `cache-on-failure: false`，release Rust/Bun 关闭，Docker 停止 GHA export，uv 显式只读策略。确认旧 workflow 运行已结束/取消，避免清完又被旧 writer 补回。
3. 新 shared-key 用 `ci-v1-*` 清晰切换；不依赖旧 job key 自动迁移。开始冷 warm 前计算并释放空间。不要在目前 8.53 GiB 上同时暖三个新的大分区。
4. 在维护窗口关闭其他分区 writer，按 pinned test → E2E mixed → Windows debug → stable registry 顺序启用/预热；每步记录压缩保存 bytes 和覆盖命令是否完成，删对应旧 archive 后再暖下一块。main 首次全部任务依然正确，miss 只能影响速度。
5. 对同分区 consumer 比较 action 输出的最终 key、路径/version、runner image 与全部已安装 toolchain；本次 unit/E2E 差异已由 image rollout 解释，后续仍可能重现。只统一本来应该相同的设置，不删除 ABI discriminator。第一阶段靠更少 target-heavy 分区和余量吸收短暂 rollout；若新增 2.80 GiB 大代不能放入余量，按第 3.2 节先停写/清旧代，不假定 1.5 GiB 能容纳所有重复。自行拆分 setup 与 cache、维护显式安全 key 的方案维护成本更高，非本次首选。
6. A/off 持续 7 天，满足容量与命中指标后再决定 registry 或 B/min；Docker 冷/热两架构数据缺失前，不宣称 B 可用。

### 6.2 仅供批准后执行的一次性删除命令

**以下命令本研究没有执行。ID 来自本次快照，执行前必须重新 list，人工确认 key/ref 与迁移阶段一致。不要 `gh cache delete --all`。** 删除不可恢复，只能重建；先存 inventory 不是备份 archive。

```sh
# 只读清单；先检查是否已有新代或正在运行的旧 writer。
gh cache list --repo Stravia-AI/StraviaPlatform --limit 100 --json id,key,ref,sizeInBytes,createdAt,lastAccessedAt

# 分阶段选用，不是一次全部执行：旧 E2E 两分区。
gh cache delete 7523948702 --repo Stravia-AI/StraviaPlatform
gh cache delete 7524330050 --repo Stravia-AI/StraviaPlatform

# 旧 unit 两代/两 toolchain；先按 job 日志确认对应关系。
gh cache delete 7523330408 --repo Stravia-AI/StraviaPlatform
gh cache delete 7523283390 --repo Stravia-AI/StraviaPlatform

# Windows 新共享 writer 启用前释放旧 browser archive。
gh cache delete 7523545144 --repo Stravia-AI/StraviaPlatform
```

Bun 与 uv 小条目无需为“清爽”删除。未来发现已关闭 PR/tag scope 或旧 `ci-v1` 环境代时，先 list 后按 **明确 ID** 删除；不在命令里笼统扫描/删除其他用户的 cache。[S11] 大版本依赖升级沿用相同的预先容量检查和受控清理过程，不能只清这一次就承诺永久稳定。

### 6.3 回滚

- 若缓存污染、错误或净耗时变差：受影响 Rust `cache: false`，Bun `no-cache: true`，uv `enable-cache: false`，Docker 去掉 import/export；正常 locked 构建仍必须通过。不要恢复旧“每 job 默认写”的策略。
- 若单分区过大：优先 stable 完全关闭、Windows 只保留 browser 覆盖、E2E 改 registry-only 或减少 mixed release 覆盖，在总预算内重新选择；这会牺牲速度，需记录而非称为等价加速。
- 若要撤销 shared key 策略，旧 key 可能已删除；不能承诺一键回到热状态。保留 main-only gate，按预算单分区重新 warm。

## 7. 后续验收指标（本研究未测量）

| 指标 | 采集方式 | 建议接受线/诊断 |
|---|---|---|
| 写入安全 | main push、PR、tag release、release dispatch/main 各一轮 action policy 与保存日志 | 只有各分区指定 writer 上传；PR/release 上传 0 bytes，无保存尝试警告；缓存不含 secrets、tokens、签名材料 |
| Exact / fallback / miss | Rust nested action 日志；uv cache-hit/key；Docker layer CACHED；按分区与事件分别统计 | 同依赖/同环境的第二轮应 exact hit；warm 可比运行目标 ≥90% exact 或有效 fallback，不能把 fallback 直接计为 exact |
| 覆盖有效性 | consumer 的 Cargo rebuild 项目、实际编译时间 | shared key 相同但每次大量重编译，说明覆盖/环境不一致；命中不等于有效加速 |
| 上传量 | archive 保存大小与 export 日志，按 run/分区计 bytes | 未改依赖且 exact hit 的 Rust 再上传应为 0；一个新有效 key 最多一个成功 writer；Docker export 单独记录，不能套 immutable Rust 判断 |
| 冷/热耗时 | 同 commit、同 runner/工具链各冷/热至少 3 次，记录 setup、restore、build/test、post-save、总关键路径 | 热运行的 restore+编译+save 总成本应低于冷构建；先报中位数、范围，不设凭空秒数。release 关闭后的成本也必须量化 |
| 7 天稳定容量 | 每日及每次依赖/toolchain/release 变更后 inventory，按 ref/partition/generation 聚合 bytes | A ≤7.80 GiB；B ≤8.40 GiB 且实际配额余量 ≥1.5 GiB；大分区至多一个有效代，小残留在 0.60 GiB 内 |
| 无 LRU thrash | 跟踪 cache ID 创建/消失、lastAccessedAt、恢复 miss、重复同 key 上传，关联手工清理日志 | 热分区在 7 天内无“未主动删除却消失→重建→再次消失”循环；单次 absent 不是 thrash 证据 |
| 代际升级演练 | 一次实际依赖或 stable 更新，记录上传前容量与旧代处理 | 先满足峰值空间再上传，不让新大 cache 触发无关热分区 eviction |
| Docker 两架构 | 分别量测 export 总存储与冷/热 layer 命中 | B 每架构 ≤0.30 GiB、合计 ≤0.60 GiB 才保留；否则 A/off 或 registry，不能只测 amd64 推断 arm64 |

GitHub 7 天未访问回收意味着低频分区自然变冷；“无 thrash”不等于“不允许过期”。cache list 的 `lastAccessedAt` 也不能替代 job 成功率/命中日志。[S1]

## 8. 安全与风险/来源矩阵

缓存内容不是经签名验证的可信发布输入；PR 能读取 base/default branch 的 cache，不能存 secrets。低信任 default-branch 事件当前有平台强制只读，但 `pull_request` 仍可在 merge ref 写 cache，所以项目仍需自己的写入 gate，而不能依赖平台自动帮忙省掉 PR 存储。[S1] `contents: read` 也不是 cache 禁写开关，Docker GHA 使用 runtime cache token。[S7]

| 关键事实或风险 | 第一方依据 | 本文状态/处置 |
|---|---|---|
| 默认 10 GB、7 天、last-access eviction、immutable key、PR merge ref、tag 隔离、低信任默认分支只读 | [S1] GitHub cache reference | 已查文档；实际 byte 配额待实施记录；本次 cache list 已读取 |
| wrapper 默认开 cache / failure save、透传输入、内置 Swatinem 版本 | [S2] 实际解析的 setup-rust-toolchain action.yml | 已查运行日志与 resolved SHA；当前固定 rust-cache v2.9.1 |
| shared-key 替换 job，且非空时 key 忽略；OS/arch/toolchain/environment/files hash；依赖清理；v2.9.2 修正版本值去重 | [S3] Swatinem v2.9.1/v2.9.2 config 与 README | 已查源码；目标 key 命中与混合覆盖未实测 |
| Bun executable、URL hash、no-cache 无单独 save-if | [S4] oven-sh 源码 | 选择非 writer 关闭，不把依赖安装当作已缓存 |
| uv 自动启用例外、显式 restore/save、key 组成 | [S5] setup-uv v10.0.1 源码 | release 显式只读；registry-only Rust 0.25 GiB 与 uv 0.05 GiB 都是预算 |
| reusable workflow 使用 caller context | [S6] GitHub reusable workflows | gate 需覆盖 tag 与 dispatch/main 实际场景，未运行验证 |
| GHA scope 共池、架构隔离、max/min、runtime token | [S7] Docker GHA backend | 当前无 Docker cache 不归因；B 容量未实测 |
| cache mounts 默认不随 GHA 保存；API v2 | [S8] Docker GitHub cache guide | 不承诺 Rust mount 跨 runner 复用；不增加 cache-dance |
| github.workflow_ref 格式 | [S9] GitHub contexts | 用于可信入口限定，不把 env 名伪装 ABI 变量 |
| registry 与 image 分离、max 导出多阶段 | [S10] Docker registry backend | registry 是后续可选资源，未创建，单独成本/权限审核 |
| 删除按 ID/ref | [S11] GitHub CLI | 仅列命令；没有执行删除 |
| profile、CRT、release 矩阵、当前 cache inputs | [L1–L6] 本地仓库文件 | 已读取；预算不是这些文件自动保证的行为 |
| mixed 2.80、Windows 2.10 是否装得下，stable 下载 cache 是否 ≤0.25 | 当前快照只提供旧 archive 总量 | **未实测假设**；超限降覆盖/停写，不默认扩池 |
| 环境 hash 意外分裂、单 writer 永远不补齐消费者差集 | [S3] + 当前 keys | 保留兼容性 hash 和容量余量；接受 consumer 差集冷编译，不增加重复预热 |

### 来源链接

- **[S1]** GitHub：[Dependency caching reference](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching)。
- **[S2]** actions-rust-lang：[本次运行解析到的 setup-rust-toolchain action.yml](https://github.com/actions-rust-lang/setup-rust-toolchain/blob/166cdcfd11aee3cb47222f9ddb555ce30ddb9659/action.yml)。
- **[S3]** Swatinem：[README](https://github.com/Swatinem/rust-cache/blob/master/README.md)、[实际运行的 v2.9.1 config.ts](https://github.com/Swatinem/rust-cache/blob/v2.9.1/src/config.ts)、[v2.9.1 restore.ts](https://github.com/Swatinem/rust-cache/blob/v2.9.1/src/restore.ts)、[v2.9.2 去重修正后的 config.ts](https://github.com/Swatinem/rust-cache/blob/v2.9.2/src/config.ts)。
- **[S4]** oven-sh：[action inputs](https://github.com/oven-sh/setup-bun/blob/main/action.yml)、[setup implementation](https://github.com/oven-sh/setup-bun/blob/main/src/action.ts)、[v2 URL key helper](https://github.com/oven-sh/setup-bun/blob/v2/src/utils.ts)。
- **[S5]** Astral：[v10.0.1 action.yml](https://github.com/astral-sh/setup-uv/blob/v10.0.1/action.yml)、[v10.0.1 restore-cache.ts](https://github.com/astral-sh/setup-uv/blob/v10.0.1/src/cache/restore-cache.ts)。
- **[S6]** GitHub：[Reusing workflow configurations — github context](https://docs.github.com/en/actions/reference/workflows-and-actions/reusing-workflow-configurations#github-context)。
- **[S7]** Docker：[GitHub Actions cache backend](https://docs.docker.com/build/cache/backends/gha/)。
- **[S8]** Docker：[Cache management with GitHub Actions](https://docs.docker.com/build/ci/github-actions/cache/)。
- **[S9]** GitHub：[Contexts reference — github context](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#github-context)。
- **[S10]** Docker：[Registry cache](https://docs.docker.com/build/cache/backends/registry/)。
- **[S11]** GitHub CLI：[gh cache delete](https://cli.github.com/manual/gh_cache_delete)。
- **[L1]** [ci.yml](../../.github/workflows/ci.yml)；**[L2]** [release.yml](../../.github/workflows/release.yml)；**[L3]** [Taskfile.yml](../../Taskfile.yml)；**[L4]** [Dockerfile](../../Dockerfile)；**[L5]** [Cargo.toml](../../Cargo.toml)；**[L6]** [.cargo/config.toml](../../.cargo/config.toml)。

本次完成的是：只读线上 inventory 复核、字节换算、源码/官方文档研究与配置设计。本文所有命中率目标、优化后容量、冷/热成本、7 天稳定性与安全 gate 运行结果，均留待后续 workflow 实施按上述步骤测量，**没有声称已实施或已验证**。
