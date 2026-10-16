-- Removes the stored account note tags. The store derives them from the addresses of the native
-- accounts.

-- ── Note tags ────────────────────────────────────────────────────────────

-- The first byte of `source` is the `NoteTagSource` discriminant. The value 0 is `Account`.
DELETE FROM tags WHERE substr(source, 1, 1) = X'00';
