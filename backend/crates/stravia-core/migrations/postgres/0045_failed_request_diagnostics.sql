ALTER TABLE inference_run_observations ADD COLUMN failure_json TEXT;
ALTER TABLE inference_run_observations ADD COLUMN request_model TEXT;
ALTER TABLE rejected_request_observations ADD COLUMN failure_json TEXT;
ALTER TABLE rejected_request_observations ADD COLUMN request_model TEXT;
ALTER TABLE rejected_request_observations ADD COLUMN api_key_id TEXT;
ALTER TABLE rejected_request_observations ADD COLUMN api_key_name TEXT;
ALTER TABLE rejected_request_observations ADD COLUMN started_at BIGINT;
ALTER TABLE rejected_request_observations ADD COLUMN duration_ms BIGINT;
CREATE INDEX inference_runs_failed_window_idx
    ON inference_run_observations(started_at DESC, id) WHERE status = 'failed';
CREATE INDEX rejected_requests_started_idx
    ON rejected_request_observations(started_at DESC, id);
