-- Prunes tags registered for output notes: they are committed via account-matched transaction
-- sync, so the tags were dead weight and, with cleanup only covering input notes, leaked one
-- row per note. A `Note`-sourced tag is a 0x01 byte plus the 32-byte details commitment; the
-- `details_commitment` columns store the same word as '0x…' hex. Tags still needed by an
-- inclusion-pending input note (Expected = 0, Unverified = 1; the SQL mirror of
-- `InputNoteRecord::is_inclusion_pending`) are kept.
DELETE FROM tags
WHERE substr(hex(source), 1, 2) = '01'
  AND EXISTS (
      SELECT 1 FROM output_notes
      WHERE output_notes.details_commitment = '0x' || lower(substr(hex(source), 3))
  )
  AND NOT EXISTS (
      SELECT 1 FROM input_notes
      WHERE input_notes.details_commitment = '0x' || lower(substr(hex(source), 3))
        AND input_notes.state_discriminant IN (0, 1)
  );

-- Stores may hold the same (tag, source) row twice (registered through both the input- and
-- output-note paths); collapse such duplicates and prevent new ones.
DELETE FROM tags
WHERE rowid NOT IN (SELECT MIN(rowid) FROM tags GROUP BY tag, source);

CREATE UNIQUE INDEX idx_tags_tag_source ON tags(tag, source);
