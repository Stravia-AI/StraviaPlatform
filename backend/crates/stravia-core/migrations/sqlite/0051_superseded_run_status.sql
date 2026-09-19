-- A run with a same-interaction continuation child was superseded when the
-- client came back, not interrupted by user input and not still waiting.
-- Older sweeps marked every waiting branch user_interrupted without checking
-- whether it had already been continued; correct the projection without
-- touching event history. Children in other interactions are new branches,
-- not continuations, and a child admitted only after the interrupt event
-- does not undo a genuine interruption.
UPDATE inference_run_observations
SET status = 'superseded',
    terminal_reason = 'superseded',
    user_interrupted = 0
WHERE (
    status = 'waiting_client'
    AND EXISTS (
        SELECT 1 FROM inference_run_observations c
        WHERE c.parent_run_id = inference_run_observations.id
          AND c.interaction_id = inference_run_observations.interaction_id)
) OR (
    status = 'user_interrupted'
    AND EXISTS (
        SELECT 1 FROM inference_run_observations c
        JOIN observation_events ce ON ce.run_id = c.id AND ce.kind = 'run_admitted'
        WHERE c.parent_run_id = inference_run_observations.id
          AND c.interaction_id = inference_run_observations.interaction_id
          AND ce.sequence < COALESCE((
              SELECT MIN(e.sequence) FROM observation_events e
              WHERE e.run_id = inference_run_observations.id
                AND e.kind = 'run_state_changed'
                AND json_extract(e.payload, '$.reason') = 'user_interrupted'),
              0))
);
