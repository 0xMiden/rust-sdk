-- Deletes the private note send queue that earlier client versions kept in the client config.

-- ── Client config ────────────────────────────────────────────────────────

-- The client no longer queues failed note transport sends, and no code reads this row.
DELETE FROM client_config WHERE name = 'note_transport_outbox';
