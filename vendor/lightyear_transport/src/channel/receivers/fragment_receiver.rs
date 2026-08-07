use alloc::{vec, vec::Vec};
use bevy_platform::collections::HashMap;

use crate::channel::receivers::error::{ChannelReceiveError, Result};
#[cfg(feature = "compression_lz4")]
use crate::packet::compression::CompressionAlgorithm;
use crate::packet::compression::{CompressionConfig, decompress_payload};
use crate::packet::error::PacketError;
use crate::packet::message::{FragmentCompression, FragmentData, MessageId};
use crate::packet::packet::FRAGMENT_SIZE;
use bytes::Bytes;
use core::time::Duration;
use lightyear_core::tick::Tick;
use tracing::trace;

const MAX_REASSEMBLED_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_INCOMPLETE_MESSAGES: usize = 64;
const MAX_RESERVED_FRAGMENT_BYTES: usize = 4 * 1024 * 1024;

/// `FragmentReceiver` is used to reconstruct fragmented messages
#[derive(Debug)]
pub struct FragmentReceiver {
    fragment_messages: HashMap<MessageId, FragmentConstructor>,
    reserved_bytes: usize,
}

impl FragmentReceiver {
    pub fn new() -> Self {
        Self {
            fragment_messages: HashMap::default(),
            reserved_bytes: 0,
        }
    }

    /// Discard all messages for which the latest fragment was received before the cleanup time
    /// (i.e. we probably lost some fragments and we will never complete the message)
    ///
    /// If we don't keep track of the last received time, we will never clean up the messages.
    pub fn cleanup(&mut self, cleanup_time: Duration) {
        self.fragment_messages.retain(|_, c| {
            c.last_received
                .map(|t| t > cleanup_time)
                .unwrap_or_else(|| true)
        });
        self.reserved_bytes = self
            .fragment_messages
            .values()
            .map(FragmentConstructor::reserved_bytes)
            .sum();
    }

    /// Receive a fragment of a FragmentData message.
    ///
    /// When we complete the final message by aggregating all fragments, we will return the
    /// `remote_sent_tick` associated with the first fragment received.
    pub fn receive_fragment(
        &mut self,
        fragment: FragmentData,
        remote_sent_tick: Tick,
        current_time: Option<Duration>,
        compression: CompressionConfig,
    ) -> Result<Option<(Tick, Bytes)>> {
        let num_fragments = fragment.num_fragments.0;
        let fragment_index = fragment.fragment_id.0;
        if num_fragments == 0 {
            return Err(ChannelReceiveError::InvalidFragmentCount { num_fragments });
        }
        if fragment_index >= num_fragments {
            return Err(ChannelReceiveError::InvalidFragmentIndex {
                fragment_index,
                num_fragments,
            });
        }

        let declared = num_fragments.checked_mul(FRAGMENT_SIZE as u64).ok_or(
            ChannelReceiveError::FragmentedMessageTooLarge {
                declared: u64::MAX,
                limit: MAX_REASSEMBLED_MESSAGE_BYTES,
            },
        )?;
        if declared > MAX_REASSEMBLED_MESSAGE_BYTES as u64 {
            return Err(ChannelReceiveError::FragmentedMessageTooLarge {
                declared,
                limit: MAX_REASSEMBLED_MESSAGE_BYTES,
            });
        }
        if fragment_index == 0 && fragment.compression.is_none() {
            return Err(ChannelReceiveError::MissingFragmentCompression);
        }

        let message_id = fragment.message_id;
        let fragment_compression = fragment.compression;
        if !self.fragment_messages.contains_key(&message_id) {
            if self.fragment_messages.len() >= MAX_INCOMPLETE_MESSAGES {
                return Err(ChannelReceiveError::TooManyIncompleteMessages {
                    limit: MAX_INCOMPLETE_MESSAGES,
                });
            }
            let reserved = usize::try_from(declared).map_err(|_| {
                ChannelReceiveError::FragmentStorageExceeded {
                    requested: usize::MAX,
                    limit: MAX_RESERVED_FRAGMENT_BYTES,
                }
            })?;
            let requested = self.reserved_bytes.checked_add(reserved).ok_or(
                ChannelReceiveError::FragmentStorageExceeded {
                    requested: usize::MAX,
                    limit: MAX_RESERVED_FRAGMENT_BYTES,
                },
            )?;
            if requested > MAX_RESERVED_FRAGMENT_BYTES {
                return Err(ChannelReceiveError::FragmentStorageExceeded {
                    requested,
                    limit: MAX_RESERVED_FRAGMENT_BYTES,
                });
            }
            self.reserved_bytes = requested;
        }
        let fragment_message = self
            .fragment_messages
            .entry(message_id)
            .or_insert_with(|| FragmentConstructor::new(remote_sent_tick, num_fragments as usize));
        if fragment_message.num_fragments != num_fragments as usize {
            return Err(ChannelReceiveError::FragmentCountMismatch {
                expected: fragment_message.num_fragments,
                actual: num_fragments as usize,
            });
        }
        if let Some(fragment_compression) = fragment_compression {
            fragment_message.set_compression(fragment_compression)?;
        } else if fragment_index == 0 {
            return Err(ChannelReceiveError::MissingFragmentCompression);
        }

        // completed the fragmented message!
        if let Some(payload) = fragment_message.receive_fragment(
            fragment_index as usize,
            fragment.bytes.as_ref(),
            current_time,
        )? {
            let fragment_compression = fragment_message
                .compression
                .ok_or(ChannelReceiveError::MissingFragmentCompression)?;
            if let Some(completed) = self.fragment_messages.remove(&message_id) {
                self.reserved_bytes = self
                    .reserved_bytes
                    .saturating_sub(completed.reserved_bytes());
            }
            let (tick, payload) = payload;
            return decompress_fragment_payload(fragment_compression, payload, compression)
                .map(|payload| Some((tick, payload)));
        }

        Ok(None)
    }
}

#[derive(Debug, Clone)]
/// Data structure to reconstruct a single fragmented message from individual fragments
pub struct FragmentConstructor {
    num_fragments: usize,
    num_received_fragments: usize,
    received: Vec<bool>,
    // bytes: Bytes,
    bytes: Vec<u8>,

    tick: Tick,
    compression: Option<FragmentCompression>,
    last_received: Option<Duration>,
}

impl FragmentConstructor {
    fn reserved_bytes(&self) -> usize {
        self.num_fragments * FRAGMENT_SIZE
    }

    pub fn new(tick: Tick, num_fragments: usize) -> Self {
        Self {
            num_fragments,
            num_received_fragments: 0,
            received: vec![false; num_fragments],
            bytes: vec![0; num_fragments * FRAGMENT_SIZE],
            tick,
            compression: None,
            last_received: None,
        }
    }

    pub fn set_compression(&mut self, compression: FragmentCompression) -> Result<()> {
        if let Some(expected) = self.compression {
            if expected != compression {
                return Err(ChannelReceiveError::FragmentCompressionMismatch {
                    expected: expected.as_str(),
                    actual: compression.as_str(),
                });
            }
        } else {
            self.compression = Some(compression);
        }
        Ok(())
    }

    pub fn receive_fragment(
        &mut self,
        fragment_index: usize,
        bytes: &[u8],
        received_time: Option<Duration>,
    ) -> Result<Option<(Tick, Bytes)>> {
        self.last_received = received_time;

        let is_last_fragment = fragment_index == self.num_fragments - 1;

        if !is_last_fragment && bytes.len() != FRAGMENT_SIZE {
            return Err(ChannelReceiveError::InvalidNonFinalFragmentSize {
                actual: bytes.len(),
                expected: FRAGMENT_SIZE,
            });
        }
        if is_last_fragment && bytes.len() > FRAGMENT_SIZE {
            return Err(ChannelReceiveError::InvalidFinalFragmentSize {
                actual: bytes.len(),
                max: FRAGMENT_SIZE,
            });
        }

        if !self.received[fragment_index] {
            self.received[fragment_index] = true;
            self.num_received_fragments += 1;

            if is_last_fragment {
                let len = (self.num_fragments - 1) * FRAGMENT_SIZE + bytes.len();
                self.bytes.resize(len, 0);
            }

            let start = fragment_index * FRAGMENT_SIZE;
            let end = start + bytes.len();
            self.bytes[start..end].copy_from_slice(bytes);
        }

        if self.num_received_fragments == self.num_fragments {
            trace!("Received all fragments!");
            let payload = core::mem::take(&mut self.bytes);
            return Ok(Some((self.tick, payload.into())));
        }

        Ok(None)
    }
}

fn decompress_fragment_payload(
    compression: FragmentCompression,
    payload: Bytes,
    config: CompressionConfig,
) -> Result<Bytes> {
    match compression {
        FragmentCompression::None => Ok(payload),
        FragmentCompression::Lz4 => {
            let config = config_for_fragment_compression(compression, config)?;
            decompress_payload(payload.as_ref(), config)
                .map(Bytes::from)
                .map_err(map_fragment_decompression_error)
        }
    }
}

fn config_for_fragment_compression(
    compression: FragmentCompression,
    config: CompressionConfig,
) -> Result<CompressionConfig> {
    match compression {
        FragmentCompression::None => Ok(config),
        FragmentCompression::Lz4 => {
            #[cfg(feature = "compression_lz4")]
            {
                if config.algorithm == Some(CompressionAlgorithm::Lz4) {
                    Ok(config)
                } else {
                    Err(ChannelReceiveError::UnsupportedFragmentCompression {
                        compression: compression.as_str(),
                    })
                }
            }
            #[cfg(not(feature = "compression_lz4"))]
            {
                Err(ChannelReceiveError::UnsupportedFragmentCompression {
                    compression: compression.as_str(),
                })
            }
        }
    }
}

fn map_fragment_decompression_error(error: PacketError) -> ChannelReceiveError {
    match error {
        PacketError::UnsupportedCompression => {
            ChannelReceiveError::UnsupportedFragmentCompression {
                compression: FragmentCompression::Lz4.as_str(),
            }
        }
        PacketError::DecompressionFailed => ChannelReceiveError::FragmentDecompressionFailed,
        PacketError::DecompressedPayloadTooLarge { actual, limit } => {
            ChannelReceiveError::FragmentDecompressedPayloadTooLarge { actual, limit }
        }
        _ => ChannelReceiveError::FragmentDecompressionFailed,
    }
}

#[cfg(test)]
mod tests {
    use crate::channel::senders::fragment_sender::FragmentSender;
    use crate::packet::message::{FragmentCompression, FragmentIndex};

    use super::*;

    #[test]
    fn test_receiver() -> Result<()> {
        let mut receiver = FragmentReceiver::new();
        let num_bytes = (FRAGMENT_SIZE as f32 * 1.5) as usize;
        let message_bytes = Bytes::from(vec![1u8; num_bytes]);
        let fragments = FragmentSender::new().build_fragments(MessageId(0), message_bytes.clone());

        assert_eq!(
            receiver.receive_fragment(
                fragments[1].clone(),
                Tick(1),
                None,
                CompressionConfig::DISABLED
            )?,
            None
        );
        assert_eq!(
            receiver.receive_fragment(
                fragments[0].clone(),
                Tick(0),
                None,
                CompressionConfig::DISABLED
            )?,
            Some((Tick(1), message_bytes.clone()))
        );
        Ok(())
    }

    fn crafted_fragment(message: u32, num_fragments: u64) -> FragmentData {
        FragmentData {
            message_id: MessageId(message),
            fragment_id: FragmentIndex(0),
            num_fragments: FragmentIndex(num_fragments),
            compression: Some(FragmentCompression::None),
            bytes: Bytes::from(vec![0; FRAGMENT_SIZE]),
        }
    }

    #[test]
    fn rejects_u64_max_fragment_declaration_before_allocation() {
        let mut receiver = FragmentReceiver::new();
        let error = receiver
            .receive_fragment(
                crafted_fragment(1, u64::MAX),
                Tick(0),
                None,
                CompressionConfig::DISABLED,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ChannelReceiveError::FragmentedMessageTooLarge { .. }
        ));
        assert!(receiver.fragment_messages.is_empty());
        assert_eq!(receiver.reserved_bytes, 0);
    }

    #[test]
    fn rejects_excessive_fragment_count_before_allocation() {
        let mut receiver = FragmentReceiver::new();
        let count = (MAX_REASSEMBLED_MESSAGE_BYTES / FRAGMENT_SIZE + 1) as u64;
        let error = receiver
            .receive_fragment(
                crafted_fragment(1, count),
                Tick(0),
                None,
                CompressionConfig::DISABLED,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ChannelReceiveError::FragmentedMessageTooLarge { .. }
        ));
        assert!(receiver.fragment_messages.is_empty());
    }

    #[test]
    fn caps_incomplete_message_flood() {
        let mut receiver = FragmentReceiver::new();
        for message in 0..MAX_INCOMPLETE_MESSAGES as u32 {
            receiver
                .receive_fragment(
                    crafted_fragment(message, 2),
                    Tick(0),
                    None,
                    CompressionConfig::DISABLED,
                )
                .unwrap();
        }
        let error = receiver
            .receive_fragment(
                crafted_fragment(MAX_INCOMPLETE_MESSAGES as u32, 2),
                Tick(0),
                None,
                CompressionConfig::DISABLED,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ChannelReceiveError::TooManyIncompleteMessages { .. }
        ));
        assert_eq!(receiver.fragment_messages.len(), MAX_INCOMPLETE_MESSAGES);
    }

    #[test]
    fn caps_reserved_fragment_storage() {
        let mut receiver = FragmentReceiver::new();
        let count = (MAX_REASSEMBLED_MESSAGE_BYTES / FRAGMENT_SIZE) as u64;
        for message in 0..4 {
            receiver
                .receive_fragment(
                    crafted_fragment(message, count),
                    Tick(0),
                    None,
                    CompressionConfig::DISABLED,
                )
                .unwrap();
        }
        let error = receiver
            .receive_fragment(
                crafted_fragment(4, count),
                Tick(0),
                None,
                CompressionConfig::DISABLED,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ChannelReceiveError::FragmentStorageExceeded { .. }
        ));
        assert_eq!(receiver.reserved_bytes, count as usize * FRAGMENT_SIZE * 4);
    }

    #[cfg(feature = "compression_lz4")]
    #[test]
    fn compressed_fragments_are_decompressed_after_reassembly() -> Result<()> {
        let compression = CompressionConfig {
            min_payload_size: 0,
            max_decompressed_payload_size: FRAGMENT_SIZE * 4,
            ..CompressionConfig::LZ4
        };
        let mut receiver = FragmentReceiver::new();
        let message_bytes = Bytes::from(vec![7u8; FRAGMENT_SIZE * 3]);
        let fragments = FragmentSender::new().build_fragments_for_message(
            MessageId(1),
            message_bytes.clone(),
            compression,
        );

        assert!(fragments.len() < 3);
        assert_eq!(fragments[0].compression, Some(FragmentCompression::Lz4));
        assert!(
            fragments
                .iter()
                .skip(1)
                .all(|fragment| fragment.compression.is_none())
        );

        let mut result = None;
        for (index, fragment) in fragments.into_iter().enumerate() {
            result = receiver.receive_fragment(fragment, Tick(index as u32), None, compression)?;
        }

        assert_eq!(result, Some((Tick(0), message_bytes)));
        Ok(())
    }
}
