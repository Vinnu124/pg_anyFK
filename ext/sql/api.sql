-- Trigger functions and bookkeeping helpers.
--
-- There is one generic trigger function per direction, configured through TG_ARGV.
-- Nothing is code-generated: every identifier reaches `format()` as a separate %I,
-- so a table or column name cannot carry SQL into the body.
--
-- All of them pin `search_path`, so a caller cannot shadow `unnest` or `cardinality`
-- with their own function and have it run inside the check.

-- ---------------------------------------------------------------- child direction

-- Fires on INSERT/UPDATE of the referencing table. Rejects the row if any element
-- of the array has no matching parent row.
CREATE FUNCTION anyfk.check_child() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_conname  name := TG_ARGV[0];
    v_ccol     name := TG_ARGV[1];
    v_pschema  name := TG_ARGV[2];
    v_ptable   name := TG_ARGV[3];
    v_pcol     name := TG_ARGV[4];
    v_changed  boolean;
    v_missing  text;
BEGIN
    -- Skip the work when an UPDATE left the array alone. Most updates do.
    IF TG_OP = 'UPDATE' THEN
        EXECUTE format('SELECT ($1).%I IS DISTINCT FROM ($2).%I', v_ccol, v_ccol)
           INTO v_changed USING NEW, OLD;
        IF NOT v_changed THEN
            RETURN NULL;
        END IF;
    END IF;

    -- Lock the referenced parent rows BEFORE checking they exist, exactly as a
    -- native foreign key does. Without this a concurrent DELETE of the parent
    -- commits alongside this INSERT and leaves a dangling reference behind -- with
    -- no error on either side. This single statement is the difference between a
    -- working constraint and a decorative one.
    EXECUTE format(
        'SELECT 1 FROM %I.%I WHERE %I = ANY (($1).%I) FOR KEY SHARE',
        v_pschema, v_ptable, v_pcol, v_ccol
    ) USING NEW;

    -- A NULL array, an empty array, and NULL elements all pass, which mirrors
    -- MATCH SIMPLE on a native foreign key.
    EXECUTE format($q$
        SELECT u.e::text
          FROM unnest(($1).%I) AS u(e)
         WHERE u.e IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM %I.%I p WHERE p.%I = u.e)
         LIMIT 1
    $q$, v_ccol, v_pschema, v_ptable, v_pcol)
       INTO v_missing USING NEW;

    IF v_missing IS NOT NULL THEN
        RAISE EXCEPTION
            'insert or update on table "%" violates foreign key constraint "%"',
            TG_TABLE_NAME, v_conname
            USING ERRCODE = 'foreign_key_violation',
                  DETAIL  = format(
                      'Element (%s) of column %I is not present in table "%I.%I".',
                      v_missing, v_ccol, v_pschema, v_ptable);
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
    v_conname  name := TG_ARGV[0];
    v_cschema  name := TG_ARGV[1];
    v_ctable   name := TG_ARGV[2];
    v_ccol     name := TG_ARGV[3];
    v_pcol     name := TG_ARGV[4];
    v_same     boolean;
    v_referenced boolean;
BEGIN
    -- An UPDATE that leaves the key alone cannot break anything.
    IF TG_OP = 'UPDATE' THEN
        EXECUTE format('SELECT ($1).%I IS NOT DISTINCT FROM ($2).%I', v_pcol, v_pcol)
           INTO v_same USING OLD, NEW;
        IF v_same THEN
            RETURN NEW;
        END IF;
    END IF;

    -- `@>` is served by a GIN index on the array column, which is why anyfk.add()
    -- warns when one is missing.
    EXECUTE format(
        'SELECT EXISTS (SELECT 1 FROM %I.%I WHERE %I @> ARRAY[($1).%I])',
        v_cschema, v_ctable, v_ccol, v_pcol
    ) INTO v_referenced USING OLD;

    IF v_referenced THEN
        RAISE EXCEPTION
            'update or delete on table "%" violates foreign key constraint "%" on table "%"',
            TG_TABLE_NAME, v_conname, v_ctable
            USING ERRCODE = 'foreign_key_violation',
                  DETAIL  = format(
                      'Key is still referenced from column %I of table "%I.%I".',
                      v_ccol, v_cschema, v_ctable);
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
CREATE FUNCTION anyfk.check_parent_truncate() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_conname  name := TG_ARGV[0];
    v_cschema  name := TG_ARGV[1];
    v_ctable   name := TG_ARGV[2];
    v_ccol     name := TG_ARGV[3];
    v_any      boolean;
BEGIN
    EXECUTE format(
        'SELECT EXISTS (SELECT 1 FROM %I.%I WHERE cardinality(%I) > 0)',
        v_cschema, v_ctable, v_ccol
    ) INTO v_any;

    IF v_any THEN
        RAISE EXCEPTION
            'cannot truncate a table referenced in a foreign key constraint "%"',
            v_conname
            USING ERRCODE = 'foreign_key_violation',
                  DETAIL  = format(
                      'Table "%I.%I" references "%s" from column %I.',
                      v_cschema, v_ctable, TG_TABLE_NAME, v_ccol),
                  HINT    = 'Truncate the referencing table first, or drop the constraint.';
    END IF;

    RETURN NULL;
END
$$;

-- ------------------------------------------------------------------- bookkeeping

-- Writing the catalog needs privileges the caller does not have, so these are
-- SECURITY DEFINER. Authorization is ownership of the referencing table -- the same
-- thing Postgres requires to create a trigger on it -- checked against
-- `session_user`, because inside a SECURITY DEFINER body `current_user` is the
-- extension owner and would always pass.
CREATE FUNCTION anyfk._require_child_owner(p_schema name, p_table name)
RETURNS void
LANGUAGE plpgsql STABLE SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_owner oid;
BEGIN
    SELECT c.relowner INTO v_owner
      FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = p_schema AND c.relname = p_table;

    IF v_owner IS NULL THEN
        RAISE EXCEPTION 'table "%.%" does not exist', p_schema, p_table
            USING ERRCODE = 'undefined_table';
    END IF;

    IF NOT pg_has_role(session_user, v_owner, 'USAGE') THEN
        RAISE EXCEPTION 'must be owner of table "%.%"', p_schema, p_table
            USING ERRCODE = 'insufficient_privilege';
    END IF;
END
$$;

CREATE FUNCTION anyfk._register(
    p_conname       name,
    p_child_schema  name,
    p_child_table   name,
    p_child_column  name,
    p_parent_schema name,
    p_parent_table  name,
    p_parent_column name,
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
    PERFORM anyfk._require_child_owner(p_child_schema, p_child_table);

    IF EXISTS (SELECT 1 FROM anyfk.constraints WHERE conname = p_conname) THEN
        RAISE EXCEPTION 'constraint "%" already exists', p_conname
            USING ERRCODE = 'duplicate_object';
    END IF;

    IF EXISTS (
        SELECT 1 FROM anyfk.constraints
         WHERE child_schema = p_child_schema
           AND child_table  = p_child_table
           AND child_column = p_child_column
           AND parent_schema = p_parent_schema
           AND parent_table  = p_parent_table
           AND parent_column = p_parent_column
    ) THEN
        RAISE EXCEPTION
            'column %I of table "%.%" already references "%.%"(%I)',
            p_child_column, p_child_schema, p_child_table,
            p_parent_schema, p_parent_table, p_parent_column
            USING ERRCODE = 'duplicate_object';
    END IF;

    INSERT INTO anyfk.constraints (
        conname, child_schema, child_table, child_column,
        parent_schema, parent_table, parent_column,
        element_type, on_delete, is_deferrable
    ) VALUES (
        p_conname, p_child_schema, p_child_table, p_child_column,
        p_parent_schema, p_parent_table, p_parent_column,
        p_element_type, p_on_delete, p_deferrable
    ) RETURNING id INTO v_id;

    RETURN v_id;
END
$$;

CREATE FUNCTION anyfk._unregister(p_conname name)
RETURNS anyfk.constraints
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_row anyfk.constraints;
BEGIN
    SELECT * INTO v_row FROM anyfk.constraints WHERE conname = p_conname;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'constraint "%" does not exist', p_conname
            USING ERRCODE = 'undefined_object';
    END IF;

    PERFORM anyfk._require_child_owner(v_row.child_schema, v_row.child_table);
    DELETE FROM anyfk.constraints WHERE conname = p_conname;
    RETURN v_row;
END
$$;

-- ------------------------------------------------------------ installing triggers
--
-- All dynamic DDL lives here rather than in Rust, so every identifier goes through
-- format()'s %I and every literal through %L. Postgres does the quoting.

CREATE FUNCTION anyfk._create_triggers(
    p_id            bigint,
    p_conname       name,
    p_child_schema  name,
    p_child_table   name,
    p_child_column  name,
    p_parent_schema name,
    p_parent_table  name,
    p_parent_column name,
    p_deferrable    boolean
)
RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    -- Child side. `UPDATE OF <col>` means the trigger is not even considered for
    -- updates that do not mention the array.
    EXECUTE format(
        'CREATE CONSTRAINT TRIGGER %I AFTER INSERT OR UPDATE OF %I ON %I.%I %s '
        'FOR EACH ROW EXECUTE FUNCTION anyfk.check_child(%L, %L, %L, %L, %L)',
        format('anyfk_%s_child', p_id),
        p_child_column, p_child_schema, p_child_table,
        CASE WHEN p_deferrable THEN 'DEFERRABLE INITIALLY DEFERRED' ELSE '' END,
        p_conname, p_child_column, p_parent_schema, p_parent_table, p_parent_column
    );

    -- Parent side: delete, and update of the referenced key.
    EXECUTE format(
        'CREATE TRIGGER %I BEFORE DELETE OR UPDATE OF %I ON %I.%I '
        'FOR EACH ROW EXECUTE FUNCTION anyfk.check_parent_delete(%L, %L, %L, %L, %L)',
        format('anyfk_%s_parent', p_id),
        p_parent_column, p_parent_schema, p_parent_table,
        p_conname, p_child_schema, p_child_table, p_child_column, p_parent_column
    );

    -- TRUNCATE bypasses row triggers entirely, so it needs its own.
    EXECUTE format(
        'CREATE TRIGGER %I BEFORE TRUNCATE ON %I.%I '
        'FOR EACH STATEMENT EXECUTE FUNCTION anyfk.check_parent_truncate(%L, %L, %L, %L)',
        format('anyfk_%s_trunc', p_id),
        p_parent_schema, p_parent_table,
        p_conname, p_child_schema, p_child_table, p_child_column
    );
END
$$;

-- DROP TRIGGER ... IF EXISTS still errors when the *table* is gone, which happens
-- constantly during cascading drops. Check first, and swallow the race.
CREATE FUNCTION anyfk._try_drop_trigger(p_name name, p_schema name, p_table name)
RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF to_regclass(format('%I.%I', p_schema, p_table)) IS NOT NULL THEN
        EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I.%I', p_name, p_schema, p_table);
    END IF;
EXCEPTION
    WHEN undefined_table OR undefined_object THEN
        NULL;
END
$$;

CREATE FUNCTION anyfk._drop_triggers(
    p_id            bigint,
    p_child_schema  name,
    p_child_table   name,
    p_parent_schema name,
    p_parent_table  name
)
RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM anyfk._try_drop_trigger(format('anyfk_%s_child', p_id),
                                    p_child_schema, p_child_table);
    PERFORM anyfk._try_drop_trigger(format('anyfk_%s_parent', p_id),
                                    p_parent_schema, p_parent_table);
    PERFORM anyfk._try_drop_trigger(format('anyfk_%s_trunc', p_id),
                                    p_parent_schema, p_parent_table);
END
$$;

-- --------------------------------------------------------------- self-cleaning
--
-- Postgres drops our triggers along with the table they sit on, but it knows
-- nothing about anyfk.constraints. Without this, dropping either table leaves an
-- orphaned catalog row and the constraint can never be re-added.
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
        SELECT schema_name, object_name, object_identity, object_type
          FROM pg_event_trigger_dropped_objects()
         WHERE object_type IN ('table', 'table column')
    LOOP
        FOR con IN
            SELECT * FROM anyfk.constraints
             WHERE (o.object_type = 'table'
                    AND ((child_schema = o.schema_name AND child_table = o.object_name)
                      OR (parent_schema = o.schema_name AND parent_table = o.object_name)))
                OR (o.object_type = 'table column'
                    AND o.object_identity = format('%s.%s.%s', child_schema, child_table, child_column))
        LOOP
            -- Every trigger has to go. A child trigger left behind would fail on
            -- each write with "relation does not exist"; a parent trigger left
            -- behind would fail on each DELETE with "column does not exist".
            PERFORM anyfk._drop_triggers(con.id, con.child_schema, con.child_table,
                                         con.parent_schema, con.parent_table);
            DELETE FROM anyfk.constraints WHERE id = con.id;
        END LOOP;
    END LOOP;
END
$$;

CREATE EVENT TRIGGER anyfk_cleanup ON sql_drop EXECUTE FUNCTION anyfk._on_drop();
