use miden_client_core::errors::RpcError;
use miden_protocol::block::BlockNumber;

/// Tracks the pagination state for block-driven endpoints.
pub(crate) struct BlockPagination {
    current_block_from: BlockNumber,
    block_to: BlockNumber,
    iterations: u32,
}

pub(crate) enum PaginationResult {
    Continue,
    Done {
        chain_tip: BlockNumber,
        block_num: BlockNumber,
    },
}

impl BlockPagination {
    /// Maximum number of pagination iterations for a single request.
    ///
    /// Protects against nodes returning inconsistent pagination data that could otherwise trigger
    /// an infinite loop.
    const MAX_ITERATIONS: u32 = 1000;

    pub(crate) fn new(block_from: BlockNumber, block_to: BlockNumber) -> Self {
        Self {
            current_block_from: block_from,
            block_to,
            iterations: 0,
        }
    }

    pub(crate) fn current_block_from(&self) -> BlockNumber {
        self.current_block_from
    }

    pub(crate) fn block_to(&self) -> BlockNumber {
        self.block_to
    }

    pub(crate) fn advance(
        &mut self,
        block_num: BlockNumber,
        chain_tip: BlockNumber,
    ) -> Result<PaginationResult, RpcError> {
        if self.iterations >= Self::MAX_ITERATIONS {
            return Err(RpcError::PaginationError(
                "too many pagination iterations, possible infinite loop".to_owned(),
            ));
        }
        self.iterations += 1;

        if block_num < self.current_block_from {
            return Err(RpcError::PaginationError(
                "invalid pagination: block_num went backwards".to_owned(),
            ));
        }

        // The node must not answer with a page that ends past the requested window. The cursor is
        // trusted downstream, so an out-of-window cursor is rejected here.
        if block_num > self.block_to {
            return Err(RpcError::PaginationError(format!(
                "invalid pagination: block_num {block_num} is past the requested block_to {}",
                self.block_to
            )));
        }

        let target_block = self.block_to.min(chain_tip);

        if block_num >= target_block {
            return Ok(PaginationResult::Done { chain_tip, block_num });
        }

        self.current_block_from = BlockNumber::from(block_num.as_u32().saturating_add(1));

        Ok(PaginationResult::Continue)
    }
}

#[cfg(test)]
mod tests {
    use miden_client_core::errors::RpcError;
    use miden_protocol::block::BlockNumber;

    use super::{BlockPagination, PaginationResult};

    #[test]
    fn block_pagination_errors_when_block_num_goes_backwards() {
        let mut pagination = BlockPagination::new(10_u32.into(), 20_u32.into());

        let res = pagination.advance(9_u32.into(), 20_u32.into());
        assert!(matches!(res, Err(RpcError::PaginationError(_))));
    }

    #[test]
    fn block_pagination_errors_when_block_num_passes_block_to() {
        let mut pagination = BlockPagination::new(10_u32.into(), 20_u32.into());

        let res = pagination.advance(21_u32.into(), 100_u32.into());
        assert!(matches!(res, Err(RpcError::PaginationError(_))));
    }

    #[test]
    fn block_pagination_errors_after_max_iterations() {
        let mut pagination = BlockPagination::new(0_u32.into(), 10_000_u32.into());
        let chain_tip: BlockNumber = 10_000_u32.into();

        for _ in 0..BlockPagination::MAX_ITERATIONS {
            let current = pagination.current_block_from();
            let res = pagination
                .advance(current, chain_tip)
                .expect("expected pagination to continue within iteration limit");
            assert!(matches!(res, PaginationResult::Continue));
        }

        let res = pagination.advance(pagination.current_block_from(), chain_tip);
        assert!(matches!(res, Err(RpcError::PaginationError(_))));
    }

    #[test]
    fn block_pagination_stops_at_min_of_block_to_and_chain_tip() {
        // block_to is beyond chain tip, so target should be chain_tip.
        let mut pagination = BlockPagination::new(0_u32.into(), 50_u32.into());

        let res = pagination
            .advance(30_u32.into(), 30_u32.into())
            .expect("expected pagination to succeed");

        assert!(matches!(
            res,
            PaginationResult::Done {
                chain_tip,
                block_num
            } if chain_tip.as_u32() == 30 && block_num.as_u32() == 30
        ));
    }

    #[test]
    fn block_pagination_advances_cursor_by_one() {
        let mut pagination = BlockPagination::new(5_u32.into(), 100_u32.into());

        let res = pagination
            .advance(5_u32.into(), 100_u32.into())
            .expect("expected pagination to succeed");
        assert!(matches!(res, PaginationResult::Continue));
        assert_eq!(pagination.current_block_from().as_u32(), 6);
    }
}
