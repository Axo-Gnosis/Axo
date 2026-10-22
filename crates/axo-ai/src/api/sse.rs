//! Incremental Server-Sent Events decoder. Port of the decoder in pi
//! `api/anthropic-messages.ts` (`decodeSseLine`, `consumeLine`, `flushSseEvent`).

/// One dispatched event. `event` is the last `event:` field; `data` joins every
/// `data:` line with `\n`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Feed raw body chunks with [`SseDecoder::push`]; call [`SseDecoder::finish`] at end of
/// body. Lines end with `\n`, `\r\n` or `\r`; lines starting with `:` are comments.
/// Works on bytes, so multi-byte characters split across chunks decode correctly.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    /// The previous line ended with `\r`; a `\n` starting the next chunk belongs to it.
    skip_lf: bool,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut chunk = chunk;
        if self.skip_lf && !chunk.is_empty() {
            self.skip_lf = false;
            if chunk[0] == b'\n' {
                chunk = &chunk[1..];
            }
        }
        self.buffer.extend_from_slice(chunk);

        let mut events = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.buffer[start..]
            .iter()
            .position(|&byte| byte == b'\n' || byte == b'\r')
        {
            let end = start + offset;
            let mut next = end + 1;
            if self.buffer[end] == b'\r' {
                match self.buffer.get(next) {
                    Some(b'\n') => next += 1,
                    Some(_) => {}
                    None => self.skip_lf = true,
                }
            }
            let line = String::from_utf8_lossy(&self.buffer[start..end]).into_owned();
            events.extend(self.line(&line));
            start = next;
        }
        self.buffer.drain(..start);
        events
    }

    /// Flush a final unterminated line and any event not yet dispatched.
    pub fn finish(mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            // A non-empty line only records a field; it never dispatches.
            self.line(&line);
        }
        self.flush()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.flush();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }

    /// Dispatch the collected fields; nothing is dispatched without a non-empty event
    /// name or at least one data line.
    fn flush(&mut self) -> Option<SseEvent> {
        if self.event.as_deref().is_none_or(str::is_empty) && self.data.is_empty() {
            return None;
        }
        Some(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}
