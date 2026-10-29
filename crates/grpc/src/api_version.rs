/// A version of the node RPC API that the client can call.
///
/// The variants go from the oldest version to the newest version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ApiVersion {
    V1,
}

impl ApiVersion {
    /// The newest version that the client supports.
    pub(crate) const NEWEST: Self = Self::V1;

    /// Returns the version before this one, or `None` for the oldest supported version.
    pub(crate) fn previous(self) -> Option<Self> {
        match self {
            Self::V1 => None,
        }
    }
}

impl Default for ApiVersion {
    fn default() -> Self {
        Self::NEWEST
    }
}
