use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: Option<String>,
}

pub struct SseDecoder {
    buffer: Vec<u8>,
    cap: usize,
}

impl SseDecoder {
    pub fn new(cap: usize) -> Self {
        Self {
            buffer: Vec::new(),
            cap,
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
            if self.buffer.len() == buffer_limit && find_delimiter(&self.buffer).is_none() {
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
        while let Some((end, delimiter_len)) = find_delimiter(&self.buffer) {
            if end > self.cap {
                return Err(frame_too_large(self.cap));
            }
            let raw = self.buffer.drain(..end + delimiter_len).collect::<Vec<_>>();
            frames.push(parse_frame(&raw[..end])?);
        }
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

fn find_delimiter(buf: &[u8]) -> Option<(usize, usize)> {
    let mut line_start = 0usize;
    while line_start < buf.len() {
        let (first_end, first_len) = find_line_ending(buf, line_start)?;
        let next_start = first_end + first_len;
        if let Some((second_end, second_len)) = find_line_ending(buf, next_start)
            && second_end == next_start
        {
            return Some((first_end, first_len + second_len));
        }
        line_start = next_start;
    }
    None
}

fn find_line_ending(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let offset = buf[from..].iter().position(|byte| *byte == b'\n')?;
    let newline = from + offset;
    if newline > from && buf[newline - 1] == b'\r' {
        Some((newline - 1, 2))
    } else {
        Some((newline, 1))
    }
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
    fn delimiter_split_after_exact_cap_is_accepted() {
        let mut decoder = SseDecoder::new(7);
        assert!(decoder.push(b"data: x").expect("first chunk").is_empty());
        assert!(decoder.push(b"\r").expect("partial delimiter").is_empty());
        let frames = decoder.push(b"\n\r\n").expect("finish delimiter");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.as_deref(), Some("x"));
    }
}
