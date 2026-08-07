//! Errors for receiving packets

pub type Result<T> = core::result::Result<T, ChannelReceiveError>;
#[derive(thiserror::Error, Debug)]
pub enum ChannelReceiveError {
    #[error("A message was received without a message ID")]
    MissingMessageId,
    #[error("fragmented message declared an invalid fragment count: {num_fragments}")]
    InvalidFragmentCount { num_fragments: u64 },
    #[error("fragmented message declares {declared} bytes, limit is {limit}")]
    FragmentedMessageTooLarge { declared: u64, limit: usize },
    #[error("too many incomplete fragmented messages: limit is {limit}")]
    TooManyIncompleteMessages { limit: usize },
    #[error("fragment receiver would reserve {requested} bytes, limit is {limit}")]
    FragmentStorageExceeded { requested: usize, limit: usize },
    #[error("fragment index {fragment_index} is outside fragment count {num_fragments}")]
    InvalidFragmentIndex {
        fragment_index: u64,
        num_fragments: u64,
    },
    #[error("fragment count changed while reassembling message: expected {expected}, got {actual}")]
    FragmentCountMismatch { expected: usize, actual: usize },
    #[error(
        "fragment compression changed while reassembling message: expected {expected}, got {actual}"
    )]
    FragmentCompressionMismatch {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("fragmented message completed before receiving compression metadata")]
    MissingFragmentCompression,
    #[error("fragment uses unsupported compression: {compression}")]
    UnsupportedFragmentCompression { compression: &'static str },
    #[error("compressed fragment payload could not be decompressed")]
    FragmentDecompressionFailed,
    #[error("decompressed fragment payload size {actual} exceeds configured limit {limit}")]
    FragmentDecompressedPayloadTooLarge { actual: usize, limit: usize },
    #[error("non-final fragment has size {actual}, expected {expected}")]
    InvalidNonFinalFragmentSize { actual: usize, expected: usize },
    #[error("final fragment has size {actual}, maximum {max}")]
    InvalidFinalFragmentSize { actual: usize, max: usize },
}
