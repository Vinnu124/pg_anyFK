CREATE SCHEMA IF NOT EXISTS anyfk;

-- One row per registered constraint. Readable by anyone, the way pg_constraint is;
-- only the SECURITY DEFINER helpers may write it.
--
-- The tables are stored as `regclass`, which holds an OID rather than a name, so
-- renaming a table does not invalidate the constraint. It is still dump-safe:
-- pg_dump writes a regclass out as the table's name and resolves it back on restore.
--
-- The columns are stored twice: the name, which is what the triggers build SQL from
-- and what survives a dump, and the attnum, which survives a rename. The event
-- trigger below keeps the two in agreement.
CREATE TABLE anyfk.constraints (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conname       name NOT NULL UNIQUE,

    child_rel     regclass NOT NULL,
    child_column  name NOT NULL,
    child_attnum  smallint NOT NULL,

    parent_rel    regclass NOT NULL,
    parent_column name NOT NULL,
    parent_attnum smallint NOT NULL,

    -- The array's element type, which must equal the parent column's type.
    element_type  text NOT NULL,

    on_delete     text NOT NULL CHECK (on_delete IN ('restrict', 'scrub', 'set_null')),
    is_deferrable boolean NOT NULL DEFAULT false,
    created_at    timestamptz NOT NULL DEFAULT now(),

    UNIQUE (child_rel, child_column, parent_rel, parent_column)
);

-- Extension-owned tables are not dumped unless marked as configuration, and losing
-- these rows would orphan every trigger we installed.
SELECT pg_catalog.pg_extension_config_dump('anyfk.constraints', '');

REVOKE ALL ON anyfk.constraints FROM PUBLIC;
GRANT USAGE ON SCHEMA anyfk TO PUBLIC;
-- The trigger functions read this table as the writing user, so SELECT must be public.
GRANT SELECT ON anyfk.constraints TO PUBLIC;
