use rand::RngExt;

/// Draws a field element uniformly at random from `rng`.
///
/// Uses rejection sampling: [`Felt::new`](crate::Felt::new) rejects any `u64` at or beyond
/// the field modulus, which keeps the result uniform over the field. The rejection
/// probability is about 2^-32.
pub fn draw_felt(rng: &mut impl rand::Rng) -> crate::Felt {
    loop {
        if let Ok(felt) = crate::Felt::new(rng.random::<u64>()) {
            return felt;
        }
    }
}

/// Draws a [`Word`](crate::Word) uniformly at random from `rng`.
///
/// Use this for note serial numbers when building notes from a plain [`rand`] generator, which
/// does not implement [`FeltRng`].
pub fn draw_word(rng: &mut impl rand::Rng) -> crate::Word {
    crate::Word::new([draw_felt(rng), draw_felt(rng), draw_felt(rng), draw_felt(rng)])
}
