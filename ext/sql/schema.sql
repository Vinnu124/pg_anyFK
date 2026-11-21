CREATE SCHEMA IF NOT EXISTS anyfk;

-- One row per registered constraint. Readable by anyone, the way pg_constraint is;
-- only the SECURITY DEFINER helpers below may write it.
CREATE TABLE anyfk.constraints (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conname       name NOT NULL UNIQUE,

    child_schema  name NOT NULL,
    child_table   name NOT NULL,
    child_column  name NOT NULL,

    parent_schema name NOT NULL,
    parent_table  name NOT NULL,
    parent_column name NOT NULL,

    -- The array's element type, which must equal the parent column's type.
    element_type  text NOT NULL,

    on_delete     text NOT NULL CHECK (on_delete IN ('restrict', 'scrub', 'set_null')),
    is_deferrable boolean NOT NULL DEFAULT false,
    created_at    timestamptz NOT NULL DEFAULT now(),

    UNIQUE (child_schema, child_table, child_column, parent_schema, parent_table, parent_column)
);

-- Extension-owned tables are not dumped unless marked as configuration, and losing
-- these rows would orphan every trigger we installed.
SELECT pg_catalog.pg_extension_config_dump('anyfk.constraints', '');

REVOKE ALL ON anyfk.constraints FROM PUBLIC;
GRANT USAGE ON SCHEMA anyfk TO PUBLIC;
GRANT SELECT ON anyfk.constraints TO PUBLIC;
