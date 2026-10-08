# Insecure Golden storage key

`storage-key.bundle` contains the deterministic test key for validator 1 from the node repository.
It contains the epoch, setup context, public key set, and one private share of a two-of-three storage key.
The test-node script supplies this bundle to the validator with `--storage-key.file`.

This key is public. Use it only in tests.
