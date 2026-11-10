use super::*;

#[derive(Clone, Default)]
pub struct HookRuntime {
    pub(super) hooks: Arc<[Arc<dyn Hook>]>,
    pub(super) tools: PlatformToolRegistry,
}

/// 只由实际成功应用的动作生成；Hook 无法自报历史仍然可信。
pub(super) enum RequestHistoryEdit {
    Splice {
        range: std::ops::Range<usize>,
        inserted: usize,
    },
    ReplaceContext,
}

pub(crate) struct RequestHookOutcome {
    pub control: HookControl,
    pub(super) history_edits: Vec<RequestHistoryEdit>,
    pub(super) refresh_context: bool,
}

impl RequestHookOutcome {
    pub(crate) fn preserves_history(&self) -> bool {
        self.history_edits.is_empty()
    }

    pub(crate) fn project_boundary(
        &self,
        candidate: &crate::generation_chain::HistoryBoundaryCandidate,
        request: &AiRequest,
        source_gap: usize,
    ) -> Option<crate::generation_chain::HistoryBoundaryCandidate> {
        let mut gaps = [Some(source_gap)];
        self.project_history_gaps(&mut gaps);
        candidate.at_projected_gap(request, gaps[0]?)
    }

    /// 被替换区间内部的 gap 没有来源证明；区间外及两端仍可精确投影。
    pub(crate) fn project_history_gaps(&self, gaps: &mut [Option<usize>]) {
        for edit in &self.history_edits {
            match edit {
                RequestHistoryEdit::Splice { range, inserted } => {
                    for gap in gaps.iter_mut().filter(|gap| gap.is_some()) {
                        let at = gap.expect("filtered present gap");
                        if at > range.start && at < range.end {
                            *gap = None;
                        } else if at >= range.end && at > range.start {
                            *gap = Some(at - range.len() + inserted);
                        }
                    }
                }
                RequestHistoryEdit::ReplaceContext => gaps.fill(None),
            }
        }
    }
}
