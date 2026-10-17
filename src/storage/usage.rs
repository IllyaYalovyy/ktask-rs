//! Provider-independent usage units. Adapters must normalize before writing model_calls.
use super::{Error, Result};

#[derive(Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

impl TokenUsage {
    /// All three input buckets are disjoint. NULL means unreported, not zero.
    pub fn from_uncached(
        input: Option<i64>,
        read: Option<i64>,
        write: Option<i64>,
        output: Option<i64>,
    ) -> Result<Self> {
        if [input, read, write, output]
            .into_iter()
            .flatten()
            .any(|n| n < 0)
        {
            return Err(Error::Conflict("token counts must not be negative".into()));
        }
        Ok(Self {
            input_tokens: input,
            cache_read_tokens: read,
            cache_write_tokens: write,
            output_tokens: output,
        })
    }

    /// For reports whose total includes BOTH cache buckets. An adapter supplies Some(0)
    /// for a bucket known not to apply; missing data must not be guessed as zero.
    pub fn from_inclusive(
        total: Option<i64>,
        read: Option<i64>,
        write: Option<i64>,
        output: Option<i64>,
    ) -> Result<Self> {
        let mut usage = Self::from_uncached(total, read, write, output)?;
        usage.input_tokens = match (total, read, write) {
            (Some(total), Some(read), Some(write)) => Some(
                total
                    .checked_sub(read)
                    .and_then(|n| n.checked_sub(write))
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| Error::Conflict("cache counts exceed inclusive input".into()))?,
            ),
            _ => None,
        };
        Ok(usage)
    }

    pub fn total_input(&self) -> Option<i64> {
        self.input_tokens?
            .checked_add(self.cache_read_tokens?)?
            .checked_add(self.cache_write_tokens?)
    }
}
