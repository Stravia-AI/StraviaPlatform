//! 已加载插件的编译产物驻留。
//!
//! 一个 `LoadedPlugin` 由两部分组成：随时可读的元数据（descriptor、身份）和
//! 占用内存的编译产物（`Resident`：机器码与预解析 blueprint）。后者只在有
//! 操作执行时才需要，因此可以在空闲后释放，并在下次使用时从 Wasmtime 持久
//! 编译缓存快速恢复；`LoadedPlugin` 句柄本身保持廉价，可被会话、路由等长期
//! 持有而不阻止释放。

use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use stravia_runtime_contract::protocol::ir::canonical::hash_bytes;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_sdk::{ErrorKind, VendorDescriptor};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;
use wasmtime::component::Component;

use super::{LoadedPlugin, StoreState, VendorRuntime};
use crate::bindings;
use crate::error::{LoadError, RuntimeError};

/// 带持久编译缓存的运行时在插件空闲这么久之后释放其编译产物。
pub(super) const IDLE_UNLOAD: Duration = Duration::from_secs(60 * 60);

pub type SourceError = Box<dyn std::error::Error + Send + Sync>;
pub type ComponentBytes = Box<dyn AsRef<[u8]> + Send>;

/// 已释放的组件重新取回字节的来源。
///
/// 返回的字节必须与插件加载时的内容身份（`LoadedPlugin::identity`）一致；
/// 运行时会校验，不一致时拒绝恢复，不会用不同代码替换已准入的插件。
#[async_trait]
pub trait ComponentSource: Send + Sync {
    async fn read(&self) -> Result<ComponentBytes, SourceError>;
}

/// 占用内存的部分：编译后的组件和绑定到其 Engine 的预解析 blueprint。
pub(super) struct Resident {
    /// 让运行时 Engine 本地的 `Weak<Component>` 编译记录在驻留期间保持可升级。
    _component: Arc<Component>,
    /// 绑定组件自身 Engine 的 blueprint，因此由另一个 `VendorRuntime` 驱动
    /// 插件时也不会改用调用方的 Engine。
    pub(super) prepared: bindings::VendorPre<StoreState>,
}

impl Resident {
    pub(super) fn new(
        component: Arc<Component>,
        prepared: bindings::VendorPre<StoreState>,
    ) -> Self {
        Self {
            _component: component,
            prepared,
        }
    }
}

struct Release {
    source: Arc<dyn ComponentSource>,
    idle: Duration,
}

struct State {
    resident: Option<Arc<Resident>>,
    /// 持有租约的操作数；非零时不释放。
    active: usize,
    last_used: Instant,
    /// 每次装入或释放递增，使过期的空闲定时任务自行退出。
    generation: u64,
}

pub(super) struct Residency {
    runtime: VendorRuntime,
    digest: [u8; 32],
    /// 组件自身报告并通过校验的 descriptor，未经 `with_descriptor` 覆盖。
    pub(super) admitted: VendorDescriptor,
    /// 凭准入记录恢复、尚未核对过组件实际 descriptor。第一次成功装入时完成核对。
    unverified: AtomicBool,
    /// `None` 表示常驻：没有来源或没有持久编译缓存时，释放后恢复需要完整
    /// 重新编译，代价高于保留内存。
    release: Option<Release>,
    state: Mutex<State>,
    /// 合并并发的恢复请求。
    reload: AsyncMutex<()>,
}

/// 操作期间对 `Resident` 的持有。归还时刷新空闲计时。
pub(super) struct ResidentLease {
    resident: Arc<Resident>,
    residency: Arc<Residency>,
}

impl Deref for ResidentLease {
    type Target = Resident;

    fn deref(&self) -> &Resident {
        &self.resident
    }
}

impl Drop for ResidentLease {
    fn drop(&mut self) {
        let mut state = self.residency.state();
        state.active -= 1;
        state.last_used = Instant::now();
    }
}

enum Tick {
    Done,
    Wait(Duration),
}

impl Residency {
    /// 常驻：不会被空闲释放。
    pub(super) fn pinned(
        runtime: VendorRuntime,
        digest: [u8; 32],
        admitted: VendorDescriptor,
        resident: Resident,
    ) -> Arc<Self> {
        Self::build(
            runtime,
            digest,
            admitted,
            false,
            None,
            Some(Arc::new(resident)),
        )
    }

    /// 凭准入记录得到、尚未装入的驻留。记录让启动不必实例化组件就能给出
    /// descriptor；首次装入时才完整校验并核对 descriptor。没有持久编译缓存时
    /// 返回 `None`：此时装入是完整编译，不值得推迟。
    pub(super) fn deferred(
        runtime: VendorRuntime,
        digest: [u8; 32],
        admitted: VendorDescriptor,
        source: Arc<dyn ComponentSource>,
    ) -> Option<Arc<Self>> {
        let idle = runtime.idle_unload?;
        Some(Self::build(
            runtime,
            digest,
            admitted,
            true,
            Some(Release { source, idle }),
            None,
        ))
    }

    fn build(
        runtime: VendorRuntime,
        digest: [u8; 32],
        admitted: VendorDescriptor,
        unverified: bool,
        release: Option<Release>,
        resident: Option<Arc<Resident>>,
    ) -> Arc<Self> {
        let resident_now = resident.is_some();
        let residency = Arc::new(Self {
            runtime,
            digest,
            admitted,
            unverified: AtomicBool::new(unverified),
            release,
            state: Mutex::new(State {
                resident,
                active: 0,
                last_used: Instant::now(),
                generation: 0,
            }),
            reload: AsyncMutex::new(()),
        });
        if resident_now {
            residency.schedule_release(0);
        }
        residency
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // 状态只有计数和时间戳，持锁线程 panic 不会留下需要回滚的不变量。
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 同一组件的新驻留，绑定重新取回字节的来源，使其可被空闲释放。已装入的
    /// 编译产物沿用，空闲计时从现在开始。运行时没有持久编译缓存时返回 `None`：
    /// 此时恢复等于完整编译，调用方应继续使用常驻的原句柄。
    pub(super) fn with_source(&self, source: Arc<dyn ComponentSource>) -> Option<Arc<Self>> {
        let idle = self.runtime.idle_unload?;
        let resident = self.state().resident.clone();
        Some(Self::build(
            self.runtime.clone(),
            self.digest,
            self.admitted.clone(),
            self.unverified.load(Ordering::Acquire),
            Some(Release { source, idle }),
            resident,
        ))
    }

    #[cfg(test)]
    pub(super) fn is_resident(&self) -> bool {
        self.state().resident.is_some()
    }

    fn try_lease(self: &Arc<Self>) -> Option<ResidentLease> {
        let mut state = self.state();
        let resident = state.resident.clone()?;
        state.active += 1;
        state.last_used = Instant::now();
        Some(ResidentLease {
            resident,
            residency: Arc::clone(self),
        })
    }

    async fn lease(self: &Arc<Self>) -> Result<ResidentLease, RuntimeError> {
        if let Some(lease) = self.try_lease() {
            return Ok(lease);
        }
        let _reloading = self.reload.lock().await;
        if let Some(lease) = self.try_lease() {
            return Ok(lease);
        }
        let resident = match self.reload_resident().await {
            Ok(resident) => resident,
            Err(error) => return Err(self.reload_failed(error)),
        };
        let (lease, generation) = {
            let mut state = self.state();
            let resident = Arc::new(resident);
            state.resident = Some(Arc::clone(&resident));
            state.generation += 1;
            state.active += 1;
            state.last_used = Instant::now();
            (
                ResidentLease {
                    resident,
                    residency: Arc::clone(self),
                },
                state.generation,
            )
        };
        self.schedule_release(generation);
        Ok(lease)
    }

    async fn reload_resident(&self) -> Result<Resident, String> {
        let Some(release) = &self.release else {
            return Err("pinned plugin lost its compiled component".into());
        };
        let bytes = release
            .source
            .read()
            .await
            .map_err(|error| format!("read component source: {error}"))?;
        let engine = self.runtime.engine.clone();
        let digest = self.digest;
        // 持久缓存命中时仍要散列、反序列化数十 MiB 字节；未命中则是完整编译。
        // 都不应占用异步工作线程。
        let compile = move || async move {
            tokio::task::spawn_blocking(move || {
                let bytes: &[u8] = (*bytes).as_ref();
                if hash_bytes(bytes) != digest {
                    return Err(LoadError::InvalidComponent(
                        "component source returned different bytes".into(),
                    ));
                }
                VendorRuntime::compile_with(&engine, bytes)
            })
            .await
            .map_err(|error| LoadError::InvalidComponent(error.to_string()))?
        };
        let component = self
            .runtime
            .compiled_component(&self.digest, compile)
            .await
            .map_err(|error| format!("compile component: {error:?}"))?;
        self.runtime
            .validate_imports(&component)
            .map_err(|error| format!("validate imports: {error:?}"))?;
        let prepared = VendorRuntime::prepare(&component)
            .map_err(|error| format!("prepare component: {error:?}"))?;
        if self.unverified.load(Ordering::Acquire) {
            // 记录只是启动时的提示：组件现在必须当场报告同一份 descriptor，
            // 才能以记录中的身份与能力运行。
            let reported = self
                .runtime
                .admit_descriptor(&prepared)
                .await
                .map_err(|error| format!("verify descriptor: {error:?}"))?;
            if reported != self.admitted {
                return Err("component descriptor differs from its admission record".into());
            }
            self.unverified.store(false, Ordering::Release);
        }
        Ok(Resident::new(component, prepared))
    }

    /// 凭记录恢复的插件首次装入就失败，说明记录已不可信（宿主校验规则变化、
    /// 记录被改动或组件已损坏）：作废它，下次启动完整校验并如实标记不可用。
    fn reload_failed(&self, mut error: String) -> RuntimeError {
        if self.unverified.load(Ordering::Acquire)
            && let Some(records) = &self.runtime.admissions
            && let Err(forget) = records.forget(&self.digest)
        {
            error.push_str(&format!(
                "; dropping the admission record also failed: {forget}"
            ));
        }
        RuntimeError::from_guest(
            ErrorKind::Trapped,
            format!("reload plugin component: {error}"),
            None,
        )
    }

    fn schedule_release(self: &Arc<Self>, generation: u64) {
        let Some(release) = &self.release else {
            return;
        };
        let idle = release.idle;
        // 定时任务只持弱引用：插件被丢弃后不延长其编译产物的生命周期。
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(residency) = weak.upgrade() else {
                    return;
                };
                let wait = match residency.release_tick(generation, idle) {
                    Tick::Done => return,
                    Tick::Wait(wait) => wait,
                };
                drop(residency);
                tokio::time::sleep(wait).await;
            }
        });
    }

    fn release_tick(&self, generation: u64, idle: Duration) -> Tick {
        let released = {
            let mut state = self.state();
            if state.generation != generation || state.resident.is_none() {
                return Tick::Done;
            }
            if state.active > 0 {
                return Tick::Wait(idle);
            }
            let elapsed = state.last_used.elapsed();
            if elapsed < idle {
                return Tick::Wait(idle - elapsed);
            }
            state.generation += 1;
            state.resident.take()
        };
        drop(released);
        Tick::Done
    }
}

impl LoadedPlugin {
    /// 返回同一组件的句柄：已装入的编译产物沿用，但不再被永久持有——空闲一段
    /// 时间后释放，下次使用时从 `source` 恢复。descriptor 与身份不受影响。
    ///
    /// 仅当运行时带持久编译缓存时生效（恢复才是缓存反序列化而非完整编译）；
    /// 否则返回常驻的等价句柄。需要在 Tokio 运行时内调用。
    pub fn with_source(&self, source: Arc<dyn ComponentSource>) -> LoadedPlugin {
        match self.inner.residency.with_source(source) {
            Some(residency) => LoadedPlugin::from_parts(
                residency,
                self.inner.descriptor.clone(),
                self.inner.identity.clone(),
            ),
            None => self.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_resident(&self) -> bool {
        self.inner.residency.is_resident()
    }
}

/// 取得执行所需的编译产物；释放过时同时服从操作的取消与截止时间。
pub(super) async fn lease(
    plugin: &LoadedPlugin,
    cancellation: &CancellationToken,
    deadline: &Deadline,
) -> Result<ResidentLease, RuntimeError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
        () = deadline.wait() => Err(RuntimeError::DeadlineExceeded),
        lease = plugin.inner.residency.lease() => lease,
    }
}

impl VendorRuntime {
    pub(super) fn compile_with(
        engine: &wasmtime::Engine,
        bytes: &[u8],
    ) -> Result<Arc<Component>, LoadError> {
        Component::from_binary(engine, bytes)
            .map(Arc::new)
            .map_err(|error| LoadError::InvalidComponent(error.to_string()))
    }

    /// 在 `directory` 保存并读取准入记录，使 [`restore`](Self::restore) 能在重启后
    /// 跳过对已校验组件的实例化。只对带持久编译缓存的运行时有意义；目录不属于
    /// Wasmtime 缓存（其清理会删除不认识的文件）。无法识别当前宿主构建时不启用。
    #[must_use]
    pub fn with_admission_records(mut self, directory: impl Into<PathBuf>) -> Self {
        self.admissions = super::admission::AdmissionRecords::new(directory.into()).map(Arc::new);
        self
    }

    /// 记录 `plugin` 已通过完整校验。应在 [`load`](Self::load) 成功后调用。
    pub async fn record_admission(&self, plugin: &LoadedPlugin) -> std::io::Result<()> {
        let (Some(records), Some(_)) = (&self.admissions, self.idle_unload) else {
            return Ok(());
        };
        let records = Arc::clone(records);
        let residency = Arc::clone(&plugin.inner.residency);
        tokio::task::spawn_blocking(move || records.write(&residency.digest, &residency.admitted))
            .await
            .map_err(std::io::Error::other)?
    }

    /// 凭准入记录取得内容摘要为 `digest`（64 位小写十六进制）的插件句柄，不读取
    /// 也不编译组件。没有有效记录、没有持久编译缓存或记录不属于当前宿主构建时
    /// 返回 `None`，调用方应读取字节并走 [`load`](Self::load)。
    ///
    /// 记录不是信任锚：句柄首次使用时，组件会被完整校验（imports 与 descriptor），
    /// 其当场报告的 descriptor 必须与记录一致，否则该次操作失败并作废记录。
    pub async fn restore(
        &self,
        digest: &str,
        source: Arc<dyn ComponentSource>,
    ) -> Option<LoadedPlugin> {
        let records = Arc::clone(self.admissions.as_ref()?);
        self.idle_unload?;
        let digest = super::admission::decode_digest(digest)?;
        let descriptor = tokio::task::spawn_blocking(move || records.read(&digest))
            .await
            .ok()??;
        // 与 load 相同的结构检查，避免损坏的记录把非法 descriptor 带进宿主。
        descriptor.validate().ok()?;
        if descriptor.canonical_format_version != super::CANONICAL_FORMAT_VERSION {
            return None;
        }
        let identity = stravia_runtime_contract::identifier::encode_digest(&digest);
        let residency = Residency::deferred(self.clone(), digest, descriptor.clone(), source)?;
        Some(LoadedPlugin::from_parts(residency, descriptor, identity))
    }
}
