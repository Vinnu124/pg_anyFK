-- Trigger functions and bookkeeping helpers.
--
-- Every trigger carries one argument: the constraint's id. The table names are read
-- from anyfk.constraints on each call rather than baked into the trigger definition,
-- which is what lets a renamed table keep working: the stored `regclass` resolves to
-- whatever the table is called now.
--
-- Nothing is code-generated. Identifiers reach `format()` as %I, or as a regclass
-- whose output function quotes and schema-qualifies them, so a table or column name
-- cannot carry SQL into the body.
--
-- All of them pin `search_path`, so a caller cannot shadow `unnest` or `cardinality`
-- with their own function and have it run inside the check. Pinning it also means a
-- regclass always renders schema-qualified.

-- ---------------------------------------------------------------- child direction

-- Fires on INSERT/UPDATE of the referencing table. Rejects the row if any element
-- of the array has no matching parent row.
CREATE FUNCTION anyfk.check_child() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    c         anyfk.constraints;
    v_changed boolean;
    v_missing text;
BEGIN
    SELECT * INTO c FROM anyfk.constraints WHERE id = TG_ARGV[0]::bigint;
    IF NOT FOUND THEN
        -- The constraint was dropped but this trigger outlived it. Allow the write
        -- rather than block the table.
        RETURN NULL;
    END IF;

    -- Skip the work when an UPDATE left the array alone. Most updates do.
    IF TG_OP = 'UPDATE' THEN
        EXECUTE format('SELECT ($1).%I IS DISTINCT FROM ($2).%I',
                       c.child_column, c.child_column)
           INTO v_changed USING NEW, OLD;
        IF NOT v_changed THEN
            RETURN NULL;
        END IF;
    END IF;

    -- Lock the referenced parent rows BEFORE checking they exist, exactly as a
    -- native foreign key does. Without this a concurrent DELETE of the parent
    -- commits alongside this INSERT and leaves a dangling reference behind, with no
    -- error on either side. This single statement is the difference between a
    -- working constraint and a decorative one.
    EXECUTE format(
        'SELECT 1 FROM %s WHERE %I = ANY (($1).%I) FOR KEY SHARE',
        c.parent_rel, c.parent_column, c.child_column
    ) USING NEW;

    -- A NULL array, an empty array, and NULL elements all pass, which mirrors
    -- MATCH SIMPLE on a native foreign key.
    EXECUTE format($q$
        SELECT u.e::text
          FROM unnest(($1).%I) AS u(e)
         WHERE u.e IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM %s p WHERE p.%I = u.e)
         LIMIT 1
    $q$, c.child_column, c.parent_rel, c.parent_column)
       INTO v_missing USING NEW;

    IF v_missing IS NOT NULL THEN
        RAISE EXCEPTION
            'insert or update on table "%" violates foreign key constraint "%"',
            TG_TABLE_NAME, c.conname
            USING ERRCODE = 'foreign_key_violation',
                  DETAIL  = format(
                      'Element (%s) of column %I is not present in table "%s".',
                      v_missing, c.child_column, c.parent_rel);
    END IF;

    RETURN NULL;
END
$$;

-- --------------------------------------------------------------- parent direction

-- Fires BEFORE DELETE, and BEFORE UPDATE of the referenced key, on the parent.
-- `restrict` refuses either while an array still contains the key. Covering UPDATE
-- matters: changing tags.id from 4 to 5 would otherwise leave every array holding a
-- 4 that no longer exists.
CREATE FUNCTION anyfk.check_parent_delete() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    c            anyfk.constraints;
    v_same       boolean;
    v_referenced boolean;
    v_child_name name;
BEGIN
    SELECT * INTO c FROM anyfk.constraints WHERE id = TG_ARGV[0]::bigint;
    IF NOT FOUND THEN
        IF TG_OP = 'UPDATE' THEN
            RETURN NEW;
        END IF;
        RETURN OLD;
    END IF;

    -- An UPDATE that leaves the key alone cannot break anything.
    IF TG_OP = 'UPDATE' THEN
        EXECUTE format('SELECT ($1).%I IS NOT DISTINCT FROM ($2).%I',
                       c.parent_column, c.parent_column)
           INTO v_same USING OLD, NEW;
        IF v_same THEN
            RETURN NEW;
        END IF;
    END IF;

    -- scrub: take the key out of every array holding it, then allow the delete.
    -- DELETE only. An UPDATE of the key falls through and raises below, because
    -- on_delete says nothing about what an update should do.
    --
    -- Runs as the deleting role, which therefore needs UPDATE on the referencing
    -- table. We do not elevate, since a SECURITY DEFINER trigger that rewrites an
    -- arbitrary table is an escalation route.
    IF c.on_delete = 'scrub' AND TG_OP = 'DELETE' THEN
        EXECUTE format(
            'UPDATE %s SET %I = array_remove(%I, ($1).%I) '
            'WHERE %I @> ARRAY[($1).%I]',
            c.child_rel, c.child_column, c.child_column, c.parent_column,
            c.child_column, c.parent_column
        ) USING OLD;
        RETURN OLD;
    END IF;

    -- `@>` is served by a GIN index on the array column, which is why anyfk.add()
    -- warns when one is missing.
    EXECUTE format(
        'SELECT EXISTS (SELECT 1 FROM %s WHERE %I @> ARRAY[($1).%I])',
        c.child_rel, c.child_column, c.parent_column
    ) INTO v_referenced USING OLD;

    IF v_referenced THEN
        -- The headline message names tables the way a native foreign key does, bare,
        -- while DETAIL carries the schema-qualified name. Looked up here rather than
        -- up front so the happy path pays nothing for it.
        SELECT relname INTO v_child_name FROM pg_class WHERE oid = c.child_rel;
        RAISE EXCEPTION
            'update or delete on table "%" violates foreign key constraint "%" on table "%"',
            TG_TABLE_NAME, c.conname, v_child_name
            USING ERRCODE = 'foreign_key_violation',
                  DETAIL  = format(
                      'Key is still referenced from column %I of table "%s".',
                      c.child_column, c.child_rel);
    END IF;

    IF TG_OP = 'UPDATE' THEN
        RETURN NEW;
    END IF;
    RETURN OLD;
END
$$;

-- TRUNCATE does not fire row-level DELETE triggers at all, so without this a
-- TRUNCATE of the parent would silently leave every array dangling. A native
-- foreign key refuses the same operation.
--
-- AFTER, not BEFORE: `TRUNCATE parent, child` is legal for a native foreign key, and
-- firing afterwards means the child is already empty by the time we look, so that
-- form is allowed while truncating the parent alone still fails.
CREATE FUNCTION anyfk.check_parent_truncate() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    c     anyfk.constraints;
    v_any boolean;
BEGIN
    SELECT * INTO c FROM anyfk.constraints WHERE id = TG_ARGV[0]::bigint;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;

    EXECUTE format(
        'SELECT EXISTS (SELECT 1 FROM %s WHERE cardinality(%I) > 0)',
        c.child_rel, c.child_column
    ) INTO v_any;

    IF v_any THEN
        RAISE EXCEPTION
            'cannot truncate a table referenced in a foreign key constraint "%"',
            c.conname
            USING ERRCODE = 'foreign_key_violation',
                  DETAIL  = format('Table "%s" references "%s" from column %I.',
                                   c.child_rel, TG_TABLE_NAME, c.child_column),
                  HINT    = 'Truncate both tables in one statement, or drop the constraint.';
    END IF;

    RETURN NULL;
END
$$;

-- ------------------------------------------------------------------- bookkeeping

-- Writing the catalog needs privileges the caller does not have, so these are
-- SECURITY DEFINER. Authorization is ownership of the referencing table, the same
-- thing Postgres requires to create a trigger on it, checked against `session_user`
-- because inside a SECURITY DEFINER body `current_user` is the extension owner and
-- would always pass.
CREATE FUNCTION anyfk._require_owner(p_rel regclass)
RETURNS void
LANGUAGE plpgsql STABLE SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_owner oid;
BEGIN
    SELECT relowner INTO v_owner FROM pg_class WHERE oid = p_rel;
    IF v_owner IS NULL THEN
        RAISE EXCEPTION 'table % does not exist', p_rel
            USING ERRCODE = 'undefined_table';
    END IF;
    IF NOT pg_has_role(session_user, v_owner, 'USAGE') THEN
        RAISE EXCEPTION 'must be owner of table %', p_rel
            USING ERRCODE = 'insufficient_privilege';
    END IF;
END
$$;

CREATE FUNCTION anyfk._register(
    p_conname       name,
    p_child_rel     regclass,
    p_child_column  name,
    p_child_attnum  smallint,
    p_parent_rel    regclass,
    p_parent_column name,
    p_parent_attnum smallint,
    p_element_type  text,
    p_on_delete     text,
    p_deferrable    boolean
)
RETURNS bigint
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_id bigint;
BEGIN
    PERFORM anyfk._require_owner(p_child_rel);

    IF EXISTS (SELECT 1 FROM anyfk.constraints WHERE conname = p_conname) THEN
        RAISE EXCEPTION 'constraint "%" already exists', p_conname
            USING ERRCODE = 'duplicate_object';
    END IF;

    IF EXISTS (
        SELECT 1 FROM anyfk.constraints
         WHERE child_rel = p_child_rel AND child_column = p_child_column
           AND parent_rel = p_parent_rel AND parent_column = p_parent_column
    ) THEN
        RAISE EXCEPTION 'column %I of table % already references %',
            p_child_column, p_child_rel, p_parent_rel
            USING ERRCODE = 'duplicate_object';
    END IF;

    INSERT INTO anyfk.constraints (
        conname, child_rel, child_column, child_attnum,
        parent_rel, parent_column, parent_attnum,
        element_type, on_delete, is_deferrable
    ) VALUES (
        p_conname, p_child_rel, p_child_column, p_child_attnum,
        p_parent_rel, p_parent_column, p_parent_attnum,
        p_element_type, p_on_delete, p_deferrable
    ) RETURNING id INTO v_id;

    RETURN v_id;
END
$$;

CREATE FUNCTION anyfk._unregister(p_conname name)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_child regclass;
BEGIN
    SELECT child_rel INTO v_child FROM anyfk.constraints WHERE conname = p_conname;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'constraint "%" does not exist', p_conname
            USING ERRCODE = 'undefined_object';
    END IF;

    PERFORM anyfk._require_owner(v_child);
    DELETE FROM anyfk.constraints WHERE conname = p_conname;
END
$$;

-- ------------------------------------------------------------ installing triggers
--
-- All dynamic DDL lives here rather than in Rust, so every identifier goes through
-- format()'s %I or a regclass output function. These take only the constraint id and
-- read the rest from the catalog, so there is one source of truth.

CREATE FUNCTION anyfk._create_triggers(p_id bigint)
RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    c anyfk.constraints;
BEGIN
    SELECT * INTO c FROM anyfk.constraints WHERE id = p_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'constraint % does not exist', p_id
            USING ERRCODE = 'undefined_object';
    END IF;

    -- Child side. `UPDATE OF <col>` means the trigger is not even considered for
    -- updates that do not mention the array. It also makes Postgres treat the column
    -- as a dependency, so a bare DROP COLUMN is refused.
    EXECUTE format(
        'CREATE CONSTRAINT TRIGGER %I AFTER INSERT OR UPDATE OF %I ON %s %s '
        'FOR EACH ROW EXECUTE FUNCTION anyfk.check_child(%L)',
        format('anyfk_%s_child', c.id), c.child_column, c.child_rel,
        CASE WHEN c.is_deferrable THEN 'DEFERRABLE INITIALLY DEFERRED' ELSE '' END,
        c.id
    );

    -- Parent side: delete, and update of the referenced key.
    EXECUTE format(
        'CREATE TRIGGER %I BEFORE DELETE OR UPDATE OF %I ON %s '
        'FOR EACH ROW EXECUTE FUNCTION anyfk.check_parent_delete(%L)',
        format('anyfk_%s_parent', c.id), c.parent_column, c.parent_rel, c.id
    );

    EXECUTE format(
        'CREATE TRIGGER %I AFTER TRUNCATE ON %s '
        'FOR EACH STATEMENT EXECUTE FUNCTION anyfk.check_parent_truncate(%L)',
        format('anyfk_%s_trunc', c.id), c.parent_rel, c.id
    );
END
$$;

-- DROP TRIGGER ... IF EXISTS still errors when the *table* is gone, which happens
-- constantly during cascading drops. Check first, and swallow the race.
CREATE FUNCTION anyfk._try_drop_trigger(p_name name, p_rel regclass)
RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_class WHERE oid = p_rel) THEN
        EXECUTE format('DROP TRIGGER IF EXISTS %I ON %s', p_name, p_rel);
    END IF;
EXCEPTION
    WHEN undefined_table OR undefined_object THEN
        NULL;
END
$$;

CREATE FUNCTION anyfk._drop_triggers(p_id bigint, p_child regclass, p_parent regclass)
RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM anyfk._try_drop_trigger(format('anyfk_%s_child', p_id), p_child);
    PERFORM anyfk._try_drop_trigger(format('anyfk_%s_parent', p_id), p_parent);
    PERFORM anyfk._try_drop_trigger(format('anyfk_%s_trunc', p_id), p_parent);
END
$$;

-- --------------------------------------------------------------- self-cleaning
--
-- Postgres drops our triggers along with the table they sit on, but it knows
-- nothing about anyfk.constraints. Without this, dropping either table leaves an
-- orphaned catalog row and the constraint can never be re-added.
--
-- Matching is by OID, which is exact and keeps working for a renamed table.
--
-- SECURITY DEFINER because the role running the DROP has no write access to the
-- catalog. It only ever deletes rows for objects Postgres has just dropped.
CREATE FUNCTION anyfk._on_drop() RETURNS event_trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    o   record;
    con record;
BEGIN
    FOR o IN
        SELECT objid, object_identity, object_type
          FROM pg_event_trigger_dropped_objects()
         WHERE object_type IN ('table', 'table column')
    LOOP
        FOR con IN
            SELECT * FROM anyfk.constraints
             WHERE (o.object_type = 'table'
                    AND (child_rel::oid = o.objid OR parent_rel::oid = o.objid))
                OR (o.object_type = 'table column'
                    AND o.object_identity = format('%s.%I', child_rel, child_column))
        LOOP
            -- Every trigger has to go. A child trigger left behind would fail on
            -- each write; a parent trigger left behind would fail on each DELETE
            -- with "column does not exist".
            PERFORM anyfk._drop_triggers(con.id, con.child_rel, con.parent_rel);
            DELETE FROM anyfk.constraints WHERE id = con.id;
        END LOOP;
    END LOOP;
END
$$;

CREATE EVENT TRIGGER anyfk_cleanup ON sql_drop EXECUTE FUNCTION anyfk._on_drop();

-- Renaming a column would otherwise leave the stored name pointing at nothing, and
-- every write to the child would fail. Postgres updates the trigger's own
-- `UPDATE OF <col>` clause by itself, so only the catalog needs fixing.
--
-- Both directions are repaired, and the guards make the two cases exclusive:
--   * the stored name is still there but the attnum drifted (a dump and restore can
--     shift attnums when a column was dropped before the dump) -> trust the name
--   * the stored name is gone -> it was renamed, so trust the attnum
CREATE FUNCTION anyfk._on_alter() RETURNS event_trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    o record;
BEGIN
    FOR o IN
        SELECT DISTINCT objid
          FROM pg_event_trigger_ddl_commands()
         WHERE classid = 'pg_class'::regclass AND command_tag = 'ALTER TABLE'
    LOOP
        -- 1. name is authoritative: realign a drifted attnum.
        UPDATE anyfk.constraints c SET child_attnum = a.attnum
          FROM pg_attribute a
         WHERE c.child_rel::oid = o.objid AND a.attrelid = o.objid
           AND a.attname = c.child_column AND a.attnum > 0 AND NOT a.attisdropped
           AND a.attnum <> c.child_attnum;

        UPDATE anyfk.constraints c SET parent_attnum = a.attnum
          FROM pg_attribute a
         WHERE c.parent_rel::oid = o.objid AND a.attrelid = o.objid
           AND a.attname = c.parent_column AND a.attnum > 0 AND NOT a.attisdropped
           AND a.attnum <> c.parent_attnum;

        -- 2. the stored name has gone, so the attnum is authoritative: adopt the
        -- name that attnum now carries.
        UPDATE anyfk.constraints c SET child_column = a.attname
          FROM pg_attribute a
         WHERE c.child_rel::oid = o.objid AND a.attrelid = o.objid
           AND a.attnum = c.child_attnum AND NOT a.attisdropped
           AND a.attname <> c.child_column
           AND NOT EXISTS (SELECT 1 FROM pg_attribute x
                            WHERE x.attrelid = o.objid AND x.attname = c.child_column
                              AND x.attnum > 0 AND NOT x.attisdropped);

        UPDATE anyfk.constraints c SET parent_column = a.attname
          FROM pg_attribute a
         WHERE c.parent_rel::oid = o.objid AND a.attrelid = o.objid
           AND a.attnum = c.parent_attnum AND NOT a.attisdropped
           AND a.attname <> c.parent_column
           AND NOT EXISTS (SELECT 1 FROM pg_attribute x
                            WHERE x.attrelid = o.objid AND x.attname = c.parent_column
                              AND x.attnum > 0 AND NOT x.attisdropped);
    END LOOP;
END
$$;

CREATE EVENT TRIGGER anyfk_rename ON ddl_command_end
    WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION anyfk._on_alter();
