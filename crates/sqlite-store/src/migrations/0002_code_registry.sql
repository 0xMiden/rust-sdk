-- ── Code registry ────────────────────────────────────────────────────────

-- The MAST forests of the code registry. Each forest is stored once, also when it has many
-- procedures.
CREATE TABLE code_mast_forests (
    commitment BLOB NOT NULL,  -- MastForest::commitment()
    mast       BLOB NOT NULL,  -- serialized MastForest

    PRIMARY KEY (commitment)
) WITHOUT ROWID, STRICT;

-- The procedures of the code registry and the forest that holds each one. The code tree in the
-- forest tables has the same procedure roots.
CREATE TABLE code_procedures (
    procedure_root BLOB NOT NULL,  -- serialized procedure root
    commitment     BLOB NOT NULL,  -- commitment of the forest that holds the procedure

    PRIMARY KEY (procedure_root),
    FOREIGN KEY (commitment) REFERENCES code_mast_forests(commitment)
) WITHOUT ROWID, STRICT;

-- The debug info of the MAST forests of the code registry.
CREATE TABLE code_debug_info (
    commitment BLOB NOT NULL,  -- commitment of the forest that the debug info describes
    debug_info BLOB NOT NULL,  -- serialized PackageDebugInfo

    PRIMARY KEY (commitment),
    FOREIGN KEY (commitment) REFERENCES code_mast_forests(commitment)
) WITHOUT ROWID, STRICT;
