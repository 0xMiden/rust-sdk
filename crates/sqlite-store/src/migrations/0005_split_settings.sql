-- Splits the settings table into a table for the client data and a table for the user values.

-- ── Client config ────────────────────────────────────────────────────────

-- The data of the client, as protobuf messages. The client reaches it only through the typed store
-- methods.
CREATE TABLE client_config (
    name  TEXT NOT NULL,
    value BLOB NOT NULL,

    PRIMARY KEY (name),
    CONSTRAINT client_config_name_is_not_empty CHECK (length(name) > 0)
) WITHOUT ROWID, STRICT;

-- A sync does not deliver a protocol configuration again, so the migration keeps these rows. The
-- other client rows (scope 0) are caches that the client fetches again, or rows that no code reads.
-- The migration does not copy them.
INSERT INTO client_config (name, value)
SELECT name, value FROM settings WHERE scope = 0 AND name GLOB 'protocol_config:*';

-- ── User settings ────────────────────────────────────────────────────────

-- The values of the user of the client.
CREATE TABLE user_settings (
    name  TEXT NOT NULL,
    value BLOB NOT NULL,

    PRIMARY KEY (name),
    CONSTRAINT user_setting_name_is_not_empty CHECK (length(name) > 0)
) WITHOUT ROWID, STRICT;

INSERT INTO user_settings (name, value) SELECT name, value FROM settings WHERE scope = 1;

-- ── Settings ─────────────────────────────────────────────────────────────

DROP TABLE settings;
