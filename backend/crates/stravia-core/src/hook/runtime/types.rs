use super::*;

#[derive(Clone, Default)]
pub struct HookRuntime {
    pub(super) hooks: Arc<[Arc<dyn Hook>]>,
    pub(super) tools: PlatformToolRegistry,
}
