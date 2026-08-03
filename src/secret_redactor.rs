//! Streaming secret redaction: scrubs known credential bytes out of child
//! process output as it arrives, holding back only enough tail bytes to catch
//! a pattern split across two reads.

const PACK_CREDENTIAL_REDACTION: &[u8] = b"[REDACTED_PACK_CREDENTIAL]";

pub(crate) struct SecretByteRedactor {
    patterns: Vec<Vec<u8>>,
    pending: Vec<u8>,
    max_pattern_len: usize,
}

impl SecretByteRedactor {
    pub(crate) fn new(patterns: Vec<Vec<u8>>) -> Self {
        let patterns = patterns
            .into_iter()
            .filter(|pattern| !pattern.is_empty())
            .collect::<Vec<_>>();
        let max_pattern_len = patterns.iter().map(Vec::len).max().unwrap_or(0);
        Self {
            patterns,
            pending: Vec::new(),
            max_pattern_len,
        }
    }

    pub(crate) fn push<F>(&mut self, bytes: &[u8], mut emit: F)
    where
        F: FnMut(&[u8]),
    {
        self.pending.extend_from_slice(bytes);
        self.drain(false, &mut emit);
    }

    pub(crate) fn finish<F>(&mut self, mut emit: F)
    where
        F: FnMut(&[u8]),
    {
        self.drain(true, &mut emit);
    }

    fn drain<F>(&mut self, finish: bool, emit: &mut F)
    where
        F: FnMut(&[u8]),
    {
        loop {
            let found = self
                .patterns
                .iter()
                .filter_map(|pattern| {
                    self.pending
                        .windows(pattern.len())
                        .position(|window| window == pattern)
                        .map(|position| (position, pattern.len()))
                })
                .min_by_key(|(position, length)| (*position, std::cmp::Reverse(*length)));
            if let Some((position, length)) = found {
                let candidate = &self.pending[position..];
                let could_extend = !finish
                    && self.patterns.iter().any(|pattern| {
                        pattern.len() > candidate.len() && pattern.starts_with(candidate)
                    });
                if could_extend {
                    emit(&self.pending[..position]);
                    self.pending.drain(..position);
                    break;
                }
                emit(&self.pending[..position]);
                emit(PACK_CREDENTIAL_REDACTION);
                self.pending.drain(..position + length);
                continue;
            }

            let keep = if finish {
                0
            } else {
                self.max_pattern_len.saturating_sub(1)
            };
            let emit_len = self.pending.len().saturating_sub(keep);
            if emit_len > 0 {
                emit(&self.pending[..emit_len]);
                self.pending.drain(..emit_len);
            }
            break;
        }
    }
}
