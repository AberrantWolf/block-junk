//! Bounded coherent revision delivery shared by spatial datasets. Validation and
//! publication are separate: an incomplete or rejected revision never leaks pages.
use crate::{
    protocol::{BoundedVec, MAX_SPATIAL_PAGE_BYTES},
    spatial::DatasetId,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
pub const MAX_REVISION_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_REVISION_PAGES: usize = MAX_REVISION_BYTES / MAX_SPATIAL_PAGE_BYTES + 1;
pub trait RevisionDataset: Send + Sync + 'static {
    type State: Serialize + serde::de::DeserializeOwned;
    const ID: DatasetId;
    const SCHEMA: u64;
    fn validate(state: &Self::State) -> Result<(), String>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RevisionFrame {
    Begin {
        dataset: DatasetId,
        schema: u64,
        sequence: u64,
        base: Option<u64>,
        bytes: u32,
        pages: u32,
        digest: [u8; 32],
    },
    Page {
        dataset: DatasetId,
        sequence: u64,
        index: u32,
        payload: BoundedVec<u8, MAX_SPATIAL_PAGE_BYTES>,
    },
    Commit {
        dataset: DatasetId,
        sequence: u64,
    },
}
impl RevisionFrame {
    pub fn dataset(&self) -> DatasetId {
        match self {
            Self::Begin { dataset, .. }
            | Self::Page { dataset, .. }
            | Self::Commit { dataset, .. } => *dataset,
        }
    }
}
struct Pending {
    sequence: u64,
    base: Option<u64>,
    bytes: usize,
    pages: u32,
    next: u32,
    digest: [u8; 32],
    payload: Vec<u8>,
}
#[derive(Default)]
pub struct RevisionAssembler {
    pub accepted_sequence: u64,
    pending: Option<Pending>,
    validating: Option<(u64, Option<u64>)>,
}
impl RevisionAssembler {
    pub fn accept(
        &mut self,
        frame: RevisionFrame,
        dataset: DatasetId,
        schema: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        let result = self.stage(frame, dataset, schema);
        if result.is_err() {
            self.pending = None;
            self.validating = None;
        }
        result
    }
    fn stage(
        &mut self,
        frame: RevisionFrame,
        dataset: DatasetId,
        schema: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        if frame.dataset() != dataset {
            return Err("revision dataset mismatch".into());
        }
        match frame {
            RevisionFrame::Begin {
                schema: actual,
                sequence,
                base,
                bytes,
                pages,
                digest,
                ..
            } => {
                if actual != schema {
                    return Err("revision schema mismatch".into());
                }
                if sequence == 0
                    || sequence <= self.accepted_sequence
                    || self.validating.is_some()
                    || self
                        .pending
                        .as_ref()
                        .is_some_and(|pending| sequence <= pending.sequence)
                {
                    return Err("stale or unvalidated revision sequence".into());
                }
                if base.is_some_and(|base| base != self.accepted_sequence) {
                    return Err("revision base is stale; replacement snapshot required".into());
                }
                if bytes as usize > MAX_REVISION_BYTES
                    || pages == 0
                    || pages as usize > MAX_REVISION_PAGES
                    || (bytes as usize).div_ceil(MAX_SPATIAL_PAGE_BYTES).max(1) != pages as usize
                {
                    return Err("revision framing exceeds bounds".into());
                }
                self.pending = Some(Pending {
                    sequence,
                    base,
                    bytes: bytes as usize,
                    pages,
                    next: 0,
                    digest,
                    payload: Vec::new(),
                });
            }
            RevisionFrame::Page {
                sequence,
                index,
                payload,
                ..
            } => {
                let pending = self.pending.as_mut().ok_or("revision page has no begin")?;
                if sequence != pending.sequence
                    || index != pending.next
                    || index >= pending.pages
                    || pending.payload.len() + payload.len() > pending.bytes
                    || (index + 1 < pending.pages && payload.len() != MAX_SPATIAL_PAGE_BYTES)
                {
                    return Err("revision page sequence or size is invalid".into());
                }
                pending.payload.extend(payload.into_inner());
                pending.next += 1;
            }
            RevisionFrame::Commit { sequence, .. } => {
                let pending = self.pending.take().ok_or("revision commit has no begin")?;
                if sequence != pending.sequence
                    || pending.next != pending.pages
                    || pending.payload.len() != pending.bytes
                    || *blake3::hash(&pending.payload).as_bytes() != pending.digest
                {
                    return Err("incomplete or corrupt spatial revision".into());
                }
                self.validating = Some((sequence, pending.base));
                return Ok(Some(pending.payload));
            }
        }
        Ok(None)
    }
    /// Call only after the dataset validator accepts the staged payload.
    pub fn validated(&mut self) -> Result<u64, String> {
        let (sequence, _) = self
            .validating
            .take()
            .ok_or("no revision awaits validation")?;
        self.accepted_sequence = sequence;
        Ok(sequence)
    }
    pub fn awaiting_replacement(&self) -> Result<bool, String> {
        self.validating
            .map(|(_, base)| base.is_none())
            .ok_or_else(|| "no revision awaits validation".into())
    }
    pub fn reject(&mut self) {
        self.pending = None;
        self.validating = None;
    }
}
pub fn encode(
    dataset: DatasetId,
    schema: u64,
    sequence: u64,
    base: Option<u64>,
    payload: Vec<u8>,
) -> Result<VecDeque<RevisionFrame>, String> {
    if sequence == 0 || payload.len() > MAX_REVISION_BYTES {
        return Err("spatial revision payload exceeds bounds".into());
    }
    let pages = payload.len().div_ceil(MAX_SPATIAL_PAGE_BYTES).max(1);
    let mut frames = VecDeque::from([RevisionFrame::Begin {
        dataset,
        schema,
        sequence,
        base,
        bytes: payload.len() as u32,
        pages: pages as u32,
        digest: *blake3::hash(&payload).as_bytes(),
    }]);
    if payload.is_empty() {
        frames.push_back(RevisionFrame::Page {
            dataset,
            sequence,
            index: 0,
            payload: BoundedVec::default(),
        });
    } else {
        for (index, chunk) in payload.chunks(MAX_SPATIAL_PAGE_BYTES).enumerate() {
            frames.push_back(RevisionFrame::Page {
                dataset,
                sequence,
                index: index as u32,
                payload: BoundedVec::new(chunk.to_vec()).map_err(|e| e.to_string())?,
            });
        }
    }
    frames.push_back(RevisionFrame::Commit { dataset, sequence });
    Ok(frames)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_are_invisible_until_complete_validated_revision_and_stale_bases_reject() {
        let bytes = vec![7; MAX_SPATIAL_PAGE_BYTES + 3];
        let mut frames = encode(DatasetId(6), 1, 1, None, bytes.clone()).unwrap();
        let mut assembly = RevisionAssembler::default();
        while frames.len() > 1 {
            assert!(
                assembly
                    .accept(frames.pop_front().unwrap(), DatasetId(6), 1)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(assembly.accepted_sequence, 0);
        }
        assert_eq!(
            assembly
                .accept(frames.pop_front().unwrap(), DatasetId(6), 1)
                .unwrap(),
            Some(bytes)
        );
        assert_eq!(assembly.accepted_sequence, 0);
        assert_eq!(assembly.validated().unwrap(), 1);
        let bad = encode(DatasetId(6), 1, 2, Some(0), vec![])
            .unwrap()
            .pop_front()
            .unwrap();
        assert!(assembly.accept(bad, DatasetId(6), 1).is_err());
        assert_eq!(assembly.accepted_sequence, 1);
        let mut replacement = encode(DatasetId(6), 1, 3, None, vec![4]).unwrap();
        let begin = replacement.pop_front().unwrap();
        assembly.accept(begin, DatasetId(6), 1).unwrap();
        assert!(
            assembly
                .accept(replacement.pop_back().unwrap(), DatasetId(6), 1)
                .is_err()
        );
        assert_eq!(assembly.accepted_sequence, 1);
    }
    #[test]
    fn corrupt_or_oversized_frames_never_advance_state() {
        let mut frames = encode(DatasetId(6), 1, 1, None, vec![1]).unwrap();
        if let RevisionFrame::Page { payload, .. } = &mut frames[1] {
            *payload = BoundedVec::new(vec![2]).unwrap();
        }
        let mut assembly = RevisionAssembler::default();
        assert!(
            assembly
                .accept(frames.pop_front().unwrap(), DatasetId(6), 1)
                .is_ok()
        );
        assert!(
            assembly
                .accept(frames.pop_front().unwrap(), DatasetId(6), 1)
                .is_ok()
        );
        assert!(
            assembly
                .accept(frames.pop_front().unwrap(), DatasetId(6), 1)
                .is_err()
        );
        assert_eq!(assembly.accepted_sequence, 0);
        assert!(
            assembly
                .accept(
                    RevisionFrame::Begin {
                        dataset: DatasetId(6),
                        schema: 1,
                        sequence: 2,
                        base: None,
                        bytes: u32::MAX,
                        pages: 1,
                        digest: [0; 32]
                    },
                    DatasetId(6),
                    1
                )
                .is_err()
        );
    }
}
