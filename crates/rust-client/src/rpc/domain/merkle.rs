use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::crypto::merkle::mmr::{Forest, MmrDelta};
use miden_protocol::crypto::merkle::{MerklePath, SparseMerklePath};

use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated as proto;

// CONSTANTS
// ================================================================================================

/// The maximum number of siblings a [`MerklePath`] can hold. `MerklePath` represents its depth as
/// a `u8`, so a longer path is not representable.
const MAX_MERKLE_PATH_SIBLINGS: usize = u8::MAX as usize;

// MERKLE PATH
// ================================================================================================

impl From<MerklePath> for proto::primitives::MerklePath {
    fn from(value: MerklePath) -> Self {
        (&value).into()
    }
}

impl From<&MerklePath> for proto::primitives::MerklePath {
    fn from(value: &MerklePath) -> Self {
        let siblings = value.nodes().iter().map(proto::primitives::Digest::from).collect();
        proto::primitives::MerklePath { siblings }
    }
}

impl TryFrom<&proto::primitives::MerklePath> for MerklePath {
    type Error = RpcConversionError;

    fn try_from(merkle_path: &proto::primitives::MerklePath) -> Result<Self, Self::Error> {
        // `MerklePath` enforces this bound with an assertion, so the count has to be checked here
        // for the conversion to stay fallible on an oversized response.
        if merkle_path.siblings.len() > MAX_MERKLE_PATH_SIBLINGS {
            return Err(RpcConversionError::InvalidField(format!(
                "MerklePath has {} siblings but at most {MAX_MERKLE_PATH_SIBLINGS} are allowed",
                merkle_path.siblings.len(),
            )));
        }

        merkle_path.siblings.iter().map(Word::try_from).collect()
    }
}

impl TryFrom<proto::primitives::MerklePath> for MerklePath {
    type Error = RpcConversionError;

    fn try_from(merkle_path: proto::primitives::MerklePath) -> Result<Self, Self::Error> {
        MerklePath::try_from(&merkle_path)
    }
}

// SPARSE MERKLE PATH

// ================================================================================================

impl From<SparseMerklePath> for proto::primitives::SparseMerklePath {
    fn from(value: SparseMerklePath) -> Self {
        let (empty_nodes_mask, siblings) = value.into_parts();

        proto::primitives::SparseMerklePath {
            empty_nodes_mask,

            siblings: siblings.into_iter().map(proto::primitives::Digest::from).collect(),
        }
    }
}

impl TryFrom<proto::primitives::SparseMerklePath> for SparseMerklePath {
    type Error = RpcConversionError;

    fn try_from(merkle_path: proto::primitives::SparseMerklePath) -> Result<Self, Self::Error> {
        Ok(SparseMerklePath::from_parts(
            merkle_path.empty_nodes_mask,
            merkle_path
                .siblings
                .into_iter()
                .map(Word::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        )?)
    }
}

// MMR DELTA
// ================================================================================================

impl TryFrom<MmrDelta> for proto::primitives::MmrDelta {
    type Error = RpcConversionError;

    fn try_from(value: MmrDelta) -> Result<Self, Self::Error> {
        let data = value.data.into_iter().map(proto::primitives::Digest::from).collect();
        Ok(proto::primitives::MmrDelta {
            forest: u64::try_from(value.forest.num_leaves())?,
            data,
        })
    }
}

impl TryFrom<proto::primitives::MmrDelta> for MmrDelta {
    type Error = RpcConversionError;

    fn try_from(value: proto::primitives::MmrDelta) -> Result<Self, Self::Error> {
        let data: Result<Vec<_>, RpcConversionError> =
            value.data.into_iter().map(Word::try_from).collect();

        let num_leaves = usize::try_from(value.forest).map_err(|_| {
            RpcConversionError::InvalidField("MmrDelta forest value exceeds usize".into())
        })?;
        Ok(MmrDelta {
            forest: Forest::new(num_leaves)
                .map_err(|_| RpcConversionError::InvalidField("MmrDelta forest invalid".into()))?,
            data: data?,
        })
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn proto_merkle_path(siblings: usize) -> proto::primitives::MerklePath {
        proto::primitives::MerklePath {
            siblings: vec![proto::primitives::Digest::default(); siblings],
        }
    }

    #[test]
    fn merkle_path_conversion_accepts_the_maximum_sibling_count() {
        let path = MerklePath::try_from(&proto_merkle_path(MAX_MERKLE_PATH_SIBLINGS))
            .expect("the maximum sibling count must convert");

        assert_eq!(path.depth(), u8::MAX);
    }

    #[test]
    fn merkle_path_conversion_rejects_an_oversized_sibling_count() {
        let oversized = proto_merkle_path(MAX_MERKLE_PATH_SIBLINGS + 1);

        assert!(MerklePath::try_from(&oversized).is_err());
    }
}
