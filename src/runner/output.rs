//! Decode each pipe independently; a read boundary need not be a UTF-8 boundary.
#[derive(Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(text) => {
                    output.push_str(text);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    output.push_str(
                        std::str::from_utf8(&self.pending[consumed..valid_end])
                            .expect("validated prefix"),
                    );
                    consumed = valid_end;
                    if let Some(invalid_length) = error.error_len() {
                        output.push('\u{fffd}');
                        consumed += invalid_length;
                    } else {
                        break; // Incomplete code point: retain it for the next read.
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        output
    }

    pub fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

#[cfg(test)]
mod tests {
    use super::Utf8Stream;

    #[test]
    fn every_chunk_boundary_preserves_unicode() {
        let text = "╭─é✔界🙂─╮\n";
        for size in 1..=text.len() {
            let mut decoder = Utf8Stream::default();
            let mut decoded = String::new();
            for chunk in text.as_bytes().chunks(size) {
                decoded.push_str(&decoder.feed(chunk));
            }
            decoded.push_str(&decoder.finish());
            assert_eq!(decoded, text, "chunk size {size}");
        }
    }

    #[test]
    fn invalid_and_unfinished_sequences_are_replaced_only_when_known() {
        let mut decoder = Utf8Stream::default();
        assert_eq!(decoder.feed(&[0xe2]), "");
        assert_eq!(decoder.feed(&[0x9c]), "");
        assert_eq!(decoder.feed(&[0x93, 0xff, b'x', 0xf0]), "✓�x");
        assert_eq!(decoder.finish(), "�");
        assert_eq!(decoder.finish(), "");
    }
}
