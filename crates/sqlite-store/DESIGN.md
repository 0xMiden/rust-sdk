# Historical State Storage Design

This document explains the design rationale behind the historical state storage model used in the SQLite store.

## Objectives

- Store change deltas for each transaction to minimize storage usage.
- Prune history before a given nonce to reduce storage.
- Roll back to the last committed state.
- Prove that an account had a specific state at a particular nonce (not implemented yet, but supported by design).

## Considered Solutions

### 1. Full snapshots per nonce

Store the full account state for every nonce. This would make rollback and pruning trivial, but storage usage would be enormous. Discarded in favor of storing only deltas.

### 2. Deltas with written-at nonce

Store deltas where each value records the nonce at which it was written, but not the nonce at which it was replaced.

Without pruning, this works well. But once pruning is considered, it becomes suboptimal because it's difficult to determine whether a value from a given nonce can be safely deleted.

To illustrate, consider this sequence of account storage changes:

| Nonce | Action |
|-------|--------|
| 0 | Account created with slots A=1, B=2, C=3 |
| 1 | Tx changes A to 10, creates D=50 |
| 2 | Tx changes B to 20 |

Under this model, the historical table would contain:

| nonce | key | value |
|-------|-----|-------|
| 0 | A | 1 |
| 0 | B | 2 |
| 0 | C | 3 |
| 1 | A | 10 |
| 1 | D | 50 |
| 2 | B | 20 |

If we prune nonces <= 1, we lose the values A=1 and C=3. If we later need to revert to nonce 2, those values are gone, and they might not exist in the latest state table either if they were replaced after nonce 2.

While a solution could be devised, it introduces complexity that is hard to reason about.

### 3. Deltas with replaced-at nonce (chosen)

Store in the historical table the **previous value** along with the nonce at which it was **replaced**. The simplified schema looks like:

```
(account_id, key, old_value, replaced_at_nonce)
```

Using the same sequence of changes, the historical table would contain:

| replaced_at_nonce | key | old_value |
|-------------------|-----|-----------|
| 1 | A | 1 |
| 1 | D | NULL (slot was new) |
| 2 | B | 2 |

At nonce 0 nothing is recorded because nothing was replaced; the account was just created.

**Why this makes pruning safe:** if we prune nonces <= 1, we delete entries with `replaced_at_nonce <= 1`, i.e., values that were replaced at or before nonce 1. These are values we would never need when reverting to nonce 2 or later, because they were already gone by then. Pruning becomes a simple `DELETE WHERE replaced_at_nonce <= threshold`.

This solution satisfies all objectives and, while still relatively simple, has enough subtlety to warrant this documentation.

## How It Works

### Write path

Before writing a value into the **latest** state, we store the previous value in the **historical** table, recording the nonce at which that value was replaced.

If no previous value existed, we store `NULL` to indicate that the slot was new at that nonce.

### Rollback

We reconstruct a previous state by applying **reverse deltas**, starting from the latest state and working backwards to the target nonce. For each historical entry between the current and target nonce:

- If the old value is non-NULL, it overwrites the current value in the latest state.
- If the old value is NULL, the entry is deleted from the latest state (it didn't exist before that nonce).
