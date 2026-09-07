use alloc::collections::BTreeSet;
use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::crypto::merkle::NodeIndex;
use miden_protocol::crypto::merkle::smt::{
    LeafIndex,
    NodeValue,
    PartialSmt,
    SMT_DEPTH,
    SmtLeaf,
    SmtProof,
    UniqueNodes,
};

use crate::rpc::domain::MissingFieldHelper;
use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated as proto;

// SMT LEAF ENTRY
// ================================================================================================

impl From<&(Word, Word)> for proto::primitives::SmtLeafEntry {
    fn from(value: &(Word, Word)) -> Self {
        proto::primitives::SmtLeafEntry {
            key: Some(value.0.into()),
            value: Some(value.1.into()),
        }
    }
}

impl TryFrom<&proto::primitives::SmtLeafEntry> for (Word, Word) {
    type Error = RpcConversionError;

    fn try_from(value: &proto::primitives::SmtLeafEntry) -> Result<Self, Self::Error> {
        let key = match value.key {
            Some(key) => key.try_into()?,
            None => return Err(proto::primitives::SmtLeafEntry::missing_field(stringify!(key))),
        };

        let value: Word = match value.value {
            Some(value) => value.try_into()?,
            None => return Err(proto::primitives::SmtLeafEntry::missing_field(stringify!(value))),
        };

        Ok((key, value))
    }
}

// SMT LEAF
// ================================================================================================

impl From<SmtLeaf> for proto::primitives::SmtLeaf {
    fn from(value: SmtLeaf) -> Self {
        (&value).into()
    }
}

impl From<&SmtLeaf> for proto::primitives::SmtLeaf {
    fn from(value: &SmtLeaf) -> Self {
        match value {
            SmtLeaf::Empty(index) => proto::primitives::SmtLeaf {
                leaf: Some(proto::primitives::smt_leaf::Leaf::EmptyLeafIndex(index.position())),
            },
            SmtLeaf::Single(entry) => proto::primitives::SmtLeaf {
                leaf: Some(proto::primitives::smt_leaf::Leaf::Single(entry.into())),
            },
            SmtLeaf::Multiple(entries) => proto::primitives::SmtLeaf {
                leaf: Some(proto::primitives::smt_leaf::Leaf::Multiple(
                    proto::primitives::SmtLeafEntryList {
                        entries: entries.iter().map(Into::into).collect(),
                    },
                )),
            },
        }
    }
}

impl TryFrom<&proto::primitives::SmtLeaf> for SmtLeaf {
    type Error = RpcConversionError;

    fn try_from(value: &proto::primitives::SmtLeaf) -> Result<Self, Self::Error> {
        match &value.leaf {
            Some(proto::primitives::smt_leaf::Leaf::EmptyLeafIndex(index)) => Ok(SmtLeaf::Empty(
                LeafIndex::<SMT_DEPTH>::new(*index)
                    .map_err(|err| RpcConversionError::InvalidField(err.to_string()))?,
            )),
            Some(proto::primitives::smt_leaf::Leaf::Single(entry)) => {
                Ok(SmtLeaf::Single(entry.try_into()?))
            },
            Some(proto::primitives::smt_leaf::Leaf::Multiple(entries)) => {
                let entries =
                    entries.entries.iter().map(TryInto::try_into).collect::<Result<_, _>>()?;
                Ok(SmtLeaf::Multiple(entries))
            },
            None => Err(proto::primitives::SmtLeaf::missing_field(stringify!(leaf))),
        }
    }
}

// SMT PROOF
// ================================================================================================

impl From<SmtProof> for proto::primitives::SmtOpening {
    fn from(value: SmtProof) -> Self {
        let (path, leaf) = value.into_parts();

        proto::primitives::SmtOpening {
            leaf: Some(leaf.into()),
            path: Some(path.into()),
        }
    }
}

// PARTIAL SMT
// ================================================================================================

impl TryFrom<proto::primitives::PartialSmt> for UniqueNodes {
    type Error = RpcConversionError;

    /// Decodes the compact partial SMT representation.
    ///
    /// The structural invariants that [`PartialSmt::from_unique_nodes`] relies on are checked here
    /// rather than left to it, so a malformed response is reported as a specific invalid field
    /// instead of an opaque reconstruction failure.
    fn try_from(value: proto::primitives::PartialSmt) -> Result<Self, Self::Error> {
        use proto::primitives::partial_smt_node::Value;

        let proto::primitives::PartialSmt {
            root,
            node_levels,
            leaves,
            value_only_leaves,
        } = value;

        let root: Word = root
            .ok_or(proto::primitives::PartialSmt::missing_field(stringify!(root)))?
            .try_into()?;

        let mut seen_depths = BTreeSet::new();
        let mut decoded_levels = Vec::with_capacity(node_levels.len());
        for level in node_levels {
            let depth = u8::try_from(level.depth)?;
            // Depth 0 is the root, which is carried separately, and `SMT_DEPTH` is the leaf level.
            // Only the strictly intermediate depths are boundary nodes.
            if depth == 0 || depth >= SMT_DEPTH {
                return Err(RpcConversionError::InvalidField(format!(
                    "partial SMT node depth {depth} must be in the range 1..{SMT_DEPTH}"
                )));
            }
            if !seen_depths.insert(depth) {
                return Err(RpcConversionError::InvalidField(format!(
                    "partial SMT contains duplicate node depth {depth}"
                )));
            }

            let mut seen_indices = BTreeSet::new();
            let mut decoded_nodes = Vec::with_capacity(level.nodes.len());
            for node in level.nodes {
                NodeIndex::new(depth, node.index)?;
                if !seen_indices.insert(node.index) {
                    return Err(RpcConversionError::InvalidField(format!(
                        "partial SMT contains duplicate node index {} at depth {depth}",
                        node.index
                    )));
                }

                let node_value = match node
                    .value
                    .ok_or(proto::primitives::PartialSmtNode::missing_field(stringify!(value)))?
                {
                    Value::Digest(digest) => NodeValue::Present(digest.try_into()?),
                    Value::EmptySubtreeRoot(true) => NodeValue::EmptySubtreeRoot,
                    Value::EmptySubtreeRoot(false) => {
                        return Err(RpcConversionError::InvalidField(
                            "partial SMT empty_subtree_root marker must be true".into(),
                        ));
                    },
                };
                decoded_nodes.push((node.index, node_value));
            }
            decoded_levels.push((depth, decoded_nodes));
        }

        let mut seen_leaf_indices = BTreeSet::new();
        let mut decoded_leaves = Vec::with_capacity(leaves.len());
        for indexed_leaf in leaves {
            if !seen_leaf_indices.insert(indexed_leaf.index) {
                return Err(RpcConversionError::InvalidField(format!(
                    "partial SMT contains duplicate leaf index {}",
                    indexed_leaf.index
                )));
            }
            let leaf: SmtLeaf = indexed_leaf
                .leaf
                .as_ref()
                .ok_or(proto::primitives::IndexedSmtLeaf::missing_field(stringify!(leaf)))?
                .try_into()?;
            decoded_leaves.push((indexed_leaf.index, leaf));
        }

        let mut seen_value_only_indices = BTreeSet::new();
        let mut decoded_value_only_leaves = Vec::with_capacity(value_only_leaves.len());
        for indexed_digest in value_only_leaves {
            if !seen_value_only_indices.insert(indexed_digest.index) {
                return Err(RpcConversionError::InvalidField(format!(
                    "partial SMT contains duplicate value-only leaf index {}",
                    indexed_digest.index
                )));
            }
            if seen_leaf_indices.contains(&indexed_digest.index) {
                return Err(RpcConversionError::InvalidField(format!(
                    "partial SMT leaf index {} has both a leaf and a value-only leaf",
                    indexed_digest.index
                )));
            }
            let digest: Word = indexed_digest
                .value
                .ok_or(proto::primitives::IndexedDigest::missing_field(stringify!(value)))?
                .try_into()?;
            decoded_value_only_leaves.push((indexed_digest.index, digest));
        }

        Ok(UniqueNodes {
            root,
            nodes: decoded_levels.into_iter().collect(),
            leaves: decoded_leaves,
            value_only_leaves: decoded_value_only_leaves,
        })
    }
}

impl TryFrom<proto::primitives::PartialSmt> for PartialSmt {
    type Error = RpcConversionError;

    fn try_from(value: proto::primitives::PartialSmt) -> Result<Self, Self::Error> {
        let unique_nodes = UniqueNodes::try_from(value)?;
        Ok(PartialSmt::from_unique_nodes(unique_nodes)?)
    }
}
