-- Moves the PSWAP lineages from the settings table to a table of their own.

-- ── PSWAP lineages ───────────────────────────────────────────────────────

-- One row for each PSWAP order that the client tracks. `record` holds the full lineage record. The
-- other columns repeat the record fields that the queries filter on.
CREATE TABLE pswap_lineages (
    order_id            INTEGER NOT NULL,  -- order ID, a u64 stored as i64
    creator_account_id  BLOB NOT NULL,     -- serialized account ID of the order creator
    current_tip_note_id BLOB NOT NULL,     -- serialized note ID of the current tip
    state               INTEGER NOT NULL,  -- lineage state (0 = Active, 1 = FullyFilled, 2 = Reclaimed)
    record              BLOB NOT NULL,     -- protobuf PswapLineageRecord

    PRIMARY KEY (order_id),
    CONSTRAINT pswap_lineage_state_is_valid CHECK (state IN (0, 1, 2))
) WITHOUT ROWID, STRICT;
CREATE INDEX idx_pswap_lineages_creator_account_id ON pswap_lineages(creator_account_id);

-- The client resolves the tip of an active lineage only. Two active lineages cannot share a tip.
CREATE UNIQUE INDEX idx_pswap_lineages_active_tip ON pswap_lineages(current_tip_note_id) WHERE state = 0;

-- ── Settings ─────────────────────────────────────────────────────────────

-- Earlier client versions kept the lineages and their tip index in the client scope (0). The
-- migration deletes these rows and does not convert them, so the client stops tracking their orders.
DELETE FROM settings WHERE scope = 0 AND (name GLOB 'pswap/order/*' OR name GLOB 'pswap/tip/*');
