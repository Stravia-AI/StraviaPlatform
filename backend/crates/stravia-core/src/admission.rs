use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

use crate::error::GatewayError;
use stravia_runtime_contract::Principal;

const WINDOW: Duration = Duration::from_secs(60);

/// 单实例 Principal RPM：检查与记录在同一锁内完成，结束或取消不返还额度。
pub(crate) struct PrincipalAdmission {
    state: Mutex<HashMap<String, PrincipalWindow>>,
}

struct PrincipalWindow {
    limit: Option<i32>,
    starts: VecDeque<Instant>,
}

impl PrincipalAdmission {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn set_limit(&self, principal_id: &str, rpm_limit: Option<i32>) {
        let mut state = self.state.lock();
        if let Some(window) = state.get_mut(principal_id) {
            // 不限期间没有可重建的历史；受限数值调整保留已准入窗口。
            if window.limit.is_none() || rpm_limit.is_none() {
                window.starts.clear();
            }
            window.limit = rpm_limit;
        } else {
            state.insert(
                principal_id.to_owned(),
                PrincipalWindow {
                    limit: rpm_limit,
                    starts: VecDeque::new(),
                },
            );
        }
    }

    pub(crate) fn remove_principal(&self, principal_id: &str) {
        self.state.lock().remove(principal_id);
    }

    pub(crate) fn acquire(
        &self,
        principal: &Principal,
        rpm_limit: Option<i32>,
    ) -> Result<(), GatewayError> {
        let mut state = self.state.lock();
        if let Some(window) = state.get_mut(principal.api_key_id()) {
            return window.acquire();
        }
        if rpm_limit.is_none() {
            return Ok(());
        }
        let mut window = PrincipalWindow {
            limit: rpm_limit,
            starts: VecDeque::new(),
        };
        let result = window.acquire();
        state.insert(principal.api_key_id().to_owned(), window);
        result
    }
}

impl PrincipalWindow {
    fn acquire(&mut self) -> Result<(), GatewayError> {
        let Some(limit) = self.limit else {
            return Ok(());
        };
        let limit = usize::try_from(limit)
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| {
                GatewayError::internal(anyhow::anyhow!(
                    "stored Principal RPM limit must be positive"
                ))
            })?;
        let now = Instant::now();
        while self
            .starts
            .front()
            .is_some_and(|start| now.duration_since(*start) >= WINDOW)
        {
            self.starts.pop_front();
        }
        if self.starts.len() >= limit {
            // 降低限额后可能需要多个旧记录离窗，不能只提示最老记录的时间。
            let expires = self.starts[self.starts.len() - limit] + WINDOW;
            let remaining = expires.duration_since(now);
            let retry_after_secs = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
            return Err(GatewayError::PrincipalRpmExceeded { retry_after_secs });
        }
        self.starts.push_back(now);
        Ok(())
    }
}
