use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: Option<String>,
}

pub struct SseDecoder {
    buffer: Vec<u8>,
    cap: usize,
    scanned: usize,
}

impl SseDecoder {
    pub fn new(cap: usize) -> Self {
        Self {
            buffer: Vec::new(),
            cap,
            scanned: 0,
        }
    }

    pub fn push(&mut self, mut chunk: &[u8]) -> Result<Vec<SseFrame>> {
        let mut frames = Vec::new();
        let buffer_limit = self.cap.saturating_add(4);
        while !chunk.is_empty() {
            let available = buffer_limit.saturating_sub(self.buffer.len());
            if available == 0 {
                return Err(frame_too_large(self.cap));
            }
            let take = available.min(chunk.len());
            self.buffer.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            frames.extend(self.drain_complete_frames()?);
            if self.buffer.len() == buffer_limit {
                return Err(frame_too_large(self.cap));
            }
        }
        Ok(frames)
    }

    pub fn finish(mut self) -> Result<Vec<SseFrame>> {
        let mut frames = self.drain_complete_frames()?;
        if !self.buffer.is_empty() {
            if self.buffer.len() > self.cap {
                return Err(frame_too_large(self.cap));
            }
            if self.buffer.iter().all(u8::is_ascii_whitespace) {
                return Ok(frames);
            }
            let trailing = std::mem::take(&mut self.buffer);
            frames.push(parse_frame(&trailing)?);
        }
        Ok(frames)
    }

    fn drain_complete_frames(&mut self) -> Result<Vec<SseFrame>> {
        let mut frames = Vec::new();
        let mut consumed = 0;
        // Retain overlap for LF/CRLF delimiters split across transport chunks.
        let mut scan_from = self.scanned.saturating_sub(3);
        while let Some((end, delimiter_len)) = find_delimiter(&self.buffer, scan_from) {
            if end - consumed > self.cap {
                return Err(frame_too_large(self.cap));
            }
            frames.push(parse_frame(&self.buffer[consumed..end])?);
            consumed = end + delimiter_len;
            scan_from = consumed;
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
        self.scanned = self.buffer.len();
        Ok(frames)
    }
}

fn frame_too_large(cap: usize) -> anyhow::Error {
    anyhow!("stream protocol error [sse/frame]: event exceeded {cap} bytes")
}

fn parse_frame(raw: &[u8]) -> Result<SseFrame> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| anyhow!("stream protocol error [sse/frame]: event is not valid UTF-8"))?;
    let mut event = None;
    let mut data_lines = Vec::new();
    for line in text.lines() {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value),
            _ => {}
        }
    }
    Ok(SseFrame {
        event,
        data: (!data_lines.is_empty()).then(|| data_lines.join("\n")),
    })
}

fn find_delimiter(buf: &[u8], mut from: usize) -> Option<(usize, usize)> {
    while let Some(offset) = buf[from..].iter().position(|byte| *byte == b'\n') {
        let newline = from + offset;
        let next = newline + 1;
        let terminal = if buf.get(next) == Some(&b'\n') {
            Some(next)
        } else if buf.get(next) == Some(&b'\r') && buf.get(next + 1) == Some(&b'\n') {
            Some(next + 1)
        } else {
            None
        };
        if let Some(terminal) = terminal {
            let end = if newline > 0 && buf[newline - 1] == b'\r' {
                newline - 1
            } else {
                newline
            };
            return Some((end, terminal + 1 - end));
        }
        from = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_chunk_with_many_small_frames_is_accepted() {
        let input = b"data: a\n\ndata: b\n\ndata: c\n\n";
        let mut decoder = SseDecoder::new(8);
        let frames = decoder.push(input).expect("decode valid frames");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].data.as_deref(), Some("c"));
    }

    #[test]
    fn oversized_incomplete_frame_is_rejected() {
        let mut decoder = SseDecoder::new(8);
        let error = decoder
            .push(b"data: abcdefghijklmnopqrstuvwxyz")
            .expect_err("oversized frame should fail");
        assert!(error.to_string().contains("event exceeded 8 bytes"));
        assert!(decoder.buffer.len() <= 12);
    }

    #[test]
    fn fragmented_mixed_delimiters_match_coalesced_frames() {
        let wire =
            b": comment\r\ndata: a\r\n\r\ndata: b\n\r\nevent: done\r\ndata: c\r\n\ndata: tail";
        let mut whole = SseDecoder::new(64);
        let mut expected = whole.push(wire).unwrap();
        expected.extend(whole.finish().unwrap());
        for chunk_size in 1..=wire.len() {
            let mut decoder = SseDecoder::new(64);
            let mut actual = Vec::new();
            for chunk in wire.chunks(chunk_size) {
                actual.extend(decoder.push(chunk).unwrap());
                assert_eq!(decoder.scanned, decoder.buffer.len());
            }
            actual.extend(decoder.finish().unwrap());
            assert_eq!(actual, expected, "chunk_size={chunk_size}");
        }
    }

    #[test]
    fn large_bytewise_frame_advances_scan_without_restarting() {
        let mut decoder = SseDecoder::new(300_006);
        for byte in b"data: ".iter().chain(std::iter::repeat_n(&b'x', 300_000)) {
            let previous = decoder.scanned;
            assert!(decoder.push(std::slice::from_ref(byte)).unwrap().is_empty());
            assert_eq!(decoder.scanned, previous + 1);
        }
        let frames = decoder.push(b"\r\n\r\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.as_ref().unwrap().len(), 300_000);
        assert!(decoder.buffer.is_empty());
        assert_eq!(decoder.scanned, 0);
    }

    #[test]
    fn coalesced_frames_enforce_each_event_cap() {
        let mut decoder = SseDecoder::new(7);
        assert!(decoder.push(b"data: a\n\ndata: bb\n\n").is_err());
        for delimiter in ["\n\n", "\r\n\r\n", "\n\r\n", "\r\n\n"] {
            let wire = format!("data: x{delimiter}");
            for cut in 0..=wire.len() {
                let mut decoder = SseDecoder::new(7);
                let mut frames = decoder.push(&wire.as_bytes()[..cut]).unwrap();
                frames.extend(decoder.push(&wire.as_bytes()[cut..]).unwrap());
                frames.extend(decoder.finish().unwrap());
                assert_eq!(frames.len(), 1, "delimiter={delimiter:?} cut={cut}");
                assert_eq!(frames[0].data.as_deref(), Some("x"));
            }
        }
    }

    #[test]
    fn delimiter_split_after_exact_cap_is_accepted() {
        let mut decoder = SseDecoder::new(7);
        assert!(decoder.push(b"data: x").expect("first chunk").is_empty());
        assert!(decoder.push(b"\r").expect("partial delimiter").is_empty());
        let frames = decoder.push(b"\n\r\n").expect("finish delimiter");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.as_deref(), Some("x"));
    }
}
