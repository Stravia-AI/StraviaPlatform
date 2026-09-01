use super::*;

#[derive(Default)]
pub(super) struct StreamReasoningNormalizer {
    pending: String,
    inside_think: bool,
    emitted_block: bool,
    current_block_started: bool,
    disabled: bool,
}

impl StreamReasoningNormalizer {
    pub(super) fn normalize(&mut self, deltas: &mut Vec<AiStreamDelta>, finish: bool) {
        if self.disabled {
            return;
        }
        let needs_work = finish
            || !self.pending.is_empty()
            || self.inside_think
            || deltas.iter().any(|delta| {
                matches!(delta, AiStreamDelta::ThinkingDelta(_))
                    || matches!(delta, AiStreamDelta::TextDelta(text) if text.contains('<'))
            });
        if !needs_work {
            return;
        }

        let input = std::mem::take(deltas);
        let mut output = Vec::with_capacity(input.len());
        for delta in input {
            match delta {
                AiStreamDelta::TextDelta(text) if !self.disabled => {
                    self.pending.push_str(&text);
                    self.drain_pending(&mut output);
                }
                AiStreamDelta::ThinkingDelta(text) => {
                    self.flush_pending_literal(&mut output);
                    self.disabled = true;
                    output.push(AiStreamDelta::ThinkingDelta(text));
                }
                terminal
                    if matches!(
                        terminal,
                        AiStreamDelta::Done { .. }
                            | AiStreamDelta::StreamError { .. }
                            | AiStreamDelta::UnexpectedEof
                    ) =>
                {
                    self.flush_terminal(&mut output);
                    output.push(terminal);
                    self.disabled = true;
                }
                other => output.push(other),
            }
        }
        if finish {
            self.flush_terminal(&mut output);
            self.disabled = true;
        }
        *deltas = output;
    }

    fn drain_pending(&mut self, output: &mut Vec<AiStreamDelta>) {
        const OPEN: &str = "<think>";
        const CLOSE: &str = "</think>";
        loop {
            let marker = if self.inside_think { CLOSE } else { OPEN };
            if let Some(index) = self.pending.find(marker) {
                let content = if self.inside_think {
                    self.pending[..index].trim().to_string()
                } else {
                    self.pending[..index].to_string()
                };
                self.emit_content(output, content);
                self.pending.drain(..index + marker.len());
                self.inside_think = !self.inside_think;
                if !self.inside_think {
                    self.current_block_started = false;
                }
                continue;
            }
            if self.inside_think {
                break;
            }
            let retained = marker_prefix_suffix_len(&self.pending, marker);
            let emitted_len = self.pending.len() - retained;
            if emitted_len > 0 {
                let content = self.pending[..emitted_len].to_string();
                self.emit_content(output, content);
                self.pending.drain(..emitted_len);
            }
            break;
        }
    }

    fn emit_content(&mut self, output: &mut Vec<AiStreamDelta>, mut content: String) {
        if self.inside_think && !self.current_block_started {
            content = content.trim_start().to_string();
        }
        if content.is_empty() {
            return;
        }
        if self.inside_think {
            if !self.current_block_started {
                if self.emitted_block {
                    push_delta_text(output, true, "\n".into());
                }
                self.current_block_started = true;
                self.emitted_block = true;
            }
            push_delta_text(output, true, content);
        } else {
            push_delta_text(output, false, content);
        }
    }

    fn flush_pending_literal(&mut self, output: &mut Vec<AiStreamDelta>) {
        if self.pending.is_empty() {
            return;
        }
        let prefix = if self.inside_think { "<think>" } else { "" };
        let text = format!("{prefix}{}", std::mem::take(&mut self.pending));
        push_delta_text(output, false, text);
        self.inside_think = false;
        self.current_block_started = false;
    }

    fn flush_terminal(&mut self, output: &mut Vec<AiStreamDelta>) {
        if self.inside_think {
            let text = format!("<think>{}", std::mem::take(&mut self.pending));
            push_delta_text(output, false, text);
            self.inside_think = false;
            self.current_block_started = false;
        } else if !self.pending.is_empty() {
            let content = std::mem::take(&mut self.pending);
            push_delta_text(output, false, content);
        }
    }
}

fn marker_prefix_suffix_len(value: &str, marker: &str) -> usize {
    (1..marker.len())
        .rev()
        .find(|&length| value.ends_with(&marker[..length]))
        .unwrap_or(0)
}

fn push_delta_text(output: &mut Vec<AiStreamDelta>, reasoning: bool, content: String) {
    match (reasoning, output.last_mut()) {
        (true, Some(AiStreamDelta::ThinkingDelta(existing)))
        | (false, Some(AiStreamDelta::TextDelta(existing))) => existing.push_str(&content),
        (true, _) => output.push(AiStreamDelta::ThinkingDelta(content)),
        (false, _) => output.push(AiStreamDelta::TextDelta(content)),
    }
}
