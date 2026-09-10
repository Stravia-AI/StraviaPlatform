use super::*;

pub(super) const WEB_ACCESS_DEADLINE: Duration = Duration::from_secs(60);
pub const MAX_FETCH_TOTAL_CHARACTERS: usize = 64_000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FetchResponse {
    pub results: Vec<FetchResult>,
}

impl FetchResponse {
    pub(crate) fn is_execution_error(&self) -> bool {
        !self.results.is_empty()
            && self
                .results
                .iter()
                .all(|result| result.status == FetchStatus::Error)
    }
}
