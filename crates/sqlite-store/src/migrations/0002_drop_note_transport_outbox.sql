-- Deletes the private note send queue that earlier client versions kept in the settings table.

-- ── Settings ─────────────────────────────────────────────────────────────

-- The client no longer queues failed note transport sends, and no code reads this row. Only the
-- client scope (0) held it. A user scope row with the same name belongs to the user and stays.
DELETE FROM settings WHERE scope = 0 AND name = 'note_transport_outbox';
