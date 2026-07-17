use std::collections::VecDeque;

pub const CAPTURE_LIMIT_BYTES: usize = 1024 * 1024;
const PREFIX_LIMIT_BYTES: usize = CAPTURE_LIMIT_BYTES / 2;
const TAIL_LIMIT_BYTES: usize = CAPTURE_LIMIT_BYTES - PREFIX_LIMIT_BYTES;

#[derive(Debug)]
pub struct BoundedCapture {
    prefix: Vec<u8>,
    tail: VecDeque<u8>,
    total_bytes: usize,
}

#[derive(Debug)]
pub struct CapturedBytes {
    pub bytes: Vec<u8>,
    pub total_bytes: usize,
    pub omitted_bytes: usize,
}

impl Default for BoundedCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl BoundedCapture {
    pub fn new() -> Self {
        Self {
            prefix: Vec::with_capacity(PREFIX_LIMIT_BYTES),
            tail: VecDeque::with_capacity(TAIL_LIMIT_BYTES),
            total_bytes: 0,
        }
    }

    pub fn append(&mut self, mut bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());

        let prefix_remaining = PREFIX_LIMIT_BYTES.saturating_sub(self.prefix.len());
        let prefix_take = prefix_remaining.min(bytes.len());
        self.prefix.extend_from_slice(&bytes[..prefix_take]);
        bytes = &bytes[prefix_take..];

        for byte in bytes {
            if self.tail.len() == TAIL_LIMIT_BYTES {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
        }
    }

    pub fn finish(self) -> CapturedBytes {
        let retained_bytes = self.prefix.len().saturating_add(self.tail.len());
        let mut bytes = Vec::with_capacity(retained_bytes);
        bytes.extend_from_slice(&self.prefix);
        bytes.extend(self.tail);
        CapturedBytes {
            bytes,
            total_bytes: self.total_bytes,
            omitted_bytes: self.total_bytes.saturating_sub(retained_bytes),
        }
    }
}

pub fn render_diagnostic(bytes: &[u8], omitted_bytes: usize) -> Vec<u8> {
    if omitted_bytes == 0 {
        return bytes.to_vec();
    }

    let prefix_len = PREFIX_LIMIT_BYTES.min(bytes.len());
    let retained_tail_len = bytes.len().saturating_sub(prefix_len);
    let mut replaced_bytes = 0usize;
    let marker = loop {
        let candidate = format!(
            "\n<{} bytes omitted; {} retained bytes replaced by marker>\n",
            omitted_bytes, replaced_bytes
        )
        .into_bytes();
        let next_replaced = candidate.len().min(retained_tail_len);
        if next_replaced == replaced_bytes {
            break candidate;
        }
        replaced_bytes = next_replaced;
    };

    let visible_tail = retained_tail_len.saturating_sub(replaced_bytes);
    let mut rendered = Vec::with_capacity(bytes.len().min(CAPTURE_LIMIT_BYTES));
    rendered.extend_from_slice(&bytes[..prefix_len]);
    rendered.extend_from_slice(&marker[..marker.len().min(replaced_bytes)]);
    rendered.extend_from_slice(&bytes[bytes.len().saturating_sub(visible_tail)..]);
    rendered.truncate(CAPTURE_LIMIT_BYTES);
    rendered
}
