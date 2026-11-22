//! SQL-callable API. Catalog validation and orchestration; parsing lives in
//! `pg_anyfk_core`, and all dynamic DDL lives in `sql/api.sql` so that Postgres'
//! own `format()` does the quoting.

use crate::spi_util::{self, OrPgError};
use pg_anyfk_core::{mode::OnDelete, parent_ref::ParentRef};
use pgrx::prelude::*;

/// A resolved column on a real table.
struct Resolved {
    schema: String,
    table: String,
    column: String,
    oid: i64,
    attnum: i32,
    /// `format_type` of the column itself, e.g. `bigint[]`. For messages only.
    type_name: String,
    /// For an array column, the element type, e.g. `bigint`. `None` otherwise.
    /// For messages only.
    element_type: Option<String>,
    /// The column's own type. Compared by OID, because `format_type` renders the
    /// type modifier for a column but not for an array's element type, which made
    /// `varchar(10)[]` look incompatible with `varchar(10)`.
    type_oid: i64,
    /// The array's element type OID, or 0 when the column is not an array.
    element_type_oid: i64,
}

const RESOLVE_SQL: &str = "
    SELECT n.nspname::text, c.relname::text, a.attname::text, c.oid::bigint,
           a.attnum::integer, format_type(a.atttypid, a.atttypmod),
           CASE WHEN t.typcategory = 'A' AND t.typelem <> 0
                THEN format_type(t.typelem, NULL) END,
           c.relkind::text,
           a.atttypid::bigint,
           CASE WHEN t.typcategory = 'A' THEN t.typelem ELSE 0 END::bigint
      FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_attribute a ON a.attrelid = c.oid
      JOIN pg_type t ON t.oid = a.atttypid
     WHERE c.oid = to_regclass($1)
       AND a.attname = $2 AND a.attnum > 0 AND NOT a.attisdropped";

/// Resolve `table(column)` against the catalogs, rejecting anything that is not a
/// plain or partitioned table.
fn resolve(spec: &ParentRef, what: &str) -> Resolved {
    let found = Spi::connect(|client| {
        let t = client
            .select(
                RESOLVE_SQL,
                Some(1),
                &[spec.quoted_table().into(), spec.column.as_str().into()],
            )
            .or_pg("resolving table");
        if t.is_empty() {
            return None;
        }
        let t = t.first();
        Some((
            t.get::<String>(1).or_pg("schema")?,
            t.get::<String>(2).or_pg("table")?,
            t.get::<String>(3).or_pg("column")?,
            t.get::<i64>(4).or_pg("oid")?,
            t.get::<i32>(5).or_pg("attnum")?,
            t.get::<String>(6).or_pg("type")?,
            t.get::<String>(7).or_pg("element type"),
            t.get::<String>(8).or_pg("relkind")?,
            t.get::<i64>(9).or_pg("type oid")?,
            t.get::<i64>(10).or_pg("element type oid")?,
        ))
    });

    let (
        schema,
        table,
        column,
        oid,
        attnum,
        type_name,
        element_type,
        relkind,
        type_oid,
        element_type_oid,
    ) = match found {
        Some(v) => v,
        None => error!(
            "{what} {} does not exist, or has no column {}",
            spec.quoted_table(),
            spec.quoted_column()
        ),
    };

    if relkind != "r" && relkind != "p" {
        error!(
            "{what} {}.{} is not a plain or partitioned table (relkind {relkind})",
            schema, table
        );
    }

    Resolved {
        schema,
        table,
        column,
        oid,
        attnum,
        type_name,
        element_type,
        type_oid,
        element_type_oid,
    }
}

fn has_unique_index(r: &Resolved) -> bool {
    spi_util::one_bool(
        "SELECT EXISTS (SELECT 1 FROM pg_index i
           WHERE i.indrelid = $1::oid AND i.indisunique AND i.indisvalid
             AND i.indnkeyatts = 1 AND i.indkey[0] = $2::smallint)",
        &[&r.oid.to_string(), &r.attnum.to_string()],
    )
}

fn has_gin_index(r: &Resolved) -> bool {
    spi_util::one_bool(
        "SELECT EXISTS (SELECT 1 FROM pg_index i
           JOIN pg_class ic ON ic.oid = i.indexrelid
           JOIN pg_am am ON am.oid = ic.relam
           WHERE i.indrelid = $1::oid AND i.indisvalid
             AND am.amname = 'gin' AND i.indkey[0] = $2::smallint)",
        &[&r.oid.to_string(), &r.attnum.to_string()],
    )
}

/// SQL that finds elements with no matching parent row, grouped by element.
fn violations_sql(child: &Resolved, parent: &Resolved) -> String {
    format!(
        "SELECT u.e::text AS missing_element, count(*)::bigint AS child_rows
           FROM {cs}.{ct} c, unnest(c.{cc}) AS u(e)
          WHERE u.e IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM {ps}.{pt} p WHERE p.{pc} = u.e)
          GROUP BY 1 ORDER BY 2 DESC, 1",
        cs = pg_anyfk_core::quote_ident(&child.schema),
        ct = pg_anyfk_core::quote_ident(&child.table),
        cc = pg_anyfk_core::quote_ident(&child.column),
        ps = pg_anyfk_core::quote_ident(&parent.schema),
        pt = pg_anyfk_core::quote_ident(&parent.table),
        pc = pg_anyfk_core::quote_ident(&parent.column),
    )
}

/// The constraint id and the two table OIDs. Everything else the SQL helpers read
/// from the catalog themselves.
fn lookup(conname: &str) -> (i64, i64, i64) {
    let row = Spi::connect(|client| {
        let t = client
            .select(
                "SELECT id, child_rel::oid::bigint, parent_rel::oid::bigint
                   FROM anyfk.constraints WHERE conname = $1",
                Some(1),
                &[conname.into()],
            )
            .or_pg("reading anyfk.constraints");
        if t.is_empty() {
            return None;
        }
        let t = t.first();
        Some((
            t.get::<i64>(1).or_pg("id")?,
            t.get::<i64>(2).or_pg("child_rel")?,
            t.get::<i64>(3).or_pg("parent_rel")?,
        ))
    });

    match row {
        Some(v) => v,
        None => error!("constraint \"{conname}\" does not exist"),
    }
}

// --------------------------------------------------------------------------- API

#[pg_extern(schema = "anyfk")]
fn version() -> String {
    format!(
        "pg_anyfk {} (core {})",
        env!("CARGO_PKG_VERSION"),
        pg_anyfk_core::VERSION
    )
}

/// Add a foreign key over the elements of an array column.
///
/// Both sides are written the way a foreign key reads:
/// `anyfk.add('posts(tag_ids)', 'tags(id)')`.
#[pg_extern(schema = "anyfk")]
fn add(
    child: &str,
    parent: &str,
    on_delete: default!(&str, "'restrict'"),
    is_deferrable: default!(bool, false),
    validate: default!(bool, true),
    conname: default!(Option<&str>, "NULL"),
) -> String {
    let child_spec = match ParentRef::parse(child) {
        Ok(s) => s,
        Err(e) => error!("child: {e}"),
    };
    let parent_spec = match ParentRef::parse(parent) {
        Ok(s) => s,
        Err(e) => error!("parent: {e}"),
    };
    let mode = match OnDelete::parse(on_delete) {
        Ok(m) => m,
        Err(e) => error!("{e}"),
    };
    if mode != OnDelete::Restrict {
        error!(
            "on_delete => '{}' is not implemented yet; only 'restrict' is supported",
            mode.as_str()
        );
    }

    let c = resolve(&child_spec, "referencing table");
    let p = resolve(&parent_spec, "referenced table");

    // The referencing column must actually be an array.
    let element_type = match &c.element_type {
        Some(t) => t.clone(),
        None => error!(
            "column {} of {}.{} is {}, not an array; use a native FOREIGN KEY instead",
            c.column, c.schema, c.table, c.type_name
        ),
    };

    // Before the type check, which would otherwise report a confusing mismatch
    // between an array and its own element type.
    if c.oid == p.oid && c.attnum == p.attnum {
        error!("a column cannot reference itself");
    }

    // Exact type equality, compared by OID so that a type modifier such as
    // varchar(10) does not make a compatible pair look mismatched.
    if c.element_type_oid != p.type_oid {
        error!(
            "type mismatch: {}.{}.{} has elements of type {} but {}.{}.{} is {}",
            c.schema, c.table, c.column, element_type, p.schema, p.table, p.column, p.type_name
        );
    }

    if !has_unique_index(&p) {
        error!(
            "there is no unique constraint matching the referenced column {}.{}.{}",
            p.schema, p.table, p.column
        );
    }

    if validate {
        let bad = Spi::connect(|client| {
            client
                .select(&violations_sql(&c, &p), Some(3), &[])
                .or_pg("checking existing rows")
                .filter_map(|row| row.get::<String>(1).ok().flatten())
                .collect::<Vec<_>>()
        });
        if !bad.is_empty() {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_FOREIGN_KEY_VIOLATION,
                format!(
                    "existing rows in {}.{} violate the constraint",
                    c.schema, c.table
                ),
                format!(
                    "Elements with no match in {}.{}: {}. \
                     Use anyfk.violations() for the full list, or validate => false to skip.",
                    p.schema,
                    p.table,
                    bad.join(", ")
                )
            );
        }
    }

    let name = conname
        .map(str::to_string)
        .unwrap_or_else(|| truncate_ident(&format!("{}_{}_anyfk", c.table, c.column)));

    // Register first so the id is available for the trigger names. A failure in
    // trigger creation rolls the row back with it.
    let id = Spi::connect_mut(|client| {
        client
            .update(
                "SELECT anyfk._register($1,$2::oid::regclass,$3,$4::smallint,\
                 $5::oid::regclass,$6,$7::smallint,$8,$9,$10)",
                Some(1),
                &[
                    name.as_str().into(),
                    c.oid.into(),
                    c.column.as_str().into(),
                    c.attnum.into(),
                    p.oid.into(),
                    p.column.as_str().into(),
                    p.attnum.into(),
                    element_type.as_str().into(),
                    mode.as_str().into(),
                    is_deferrable.into(),
                ],
            )
            .or_pg("registering the constraint")
            .first()
            .get_one::<i64>()
            .or_pg("reading the new constraint id")
    })
    .unwrap_or_else(|| error!("anyfk._register returned no id"));

    Spi::connect_mut(|client| {
        client
            .update("SELECT anyfk._create_triggers($1)", Some(1), &[id.into()])
            .or_pg("creating the triggers");
    });

    // The parent-side delete check needs this to avoid a sequential scan.
    if !has_gin_index(&c) {
        ereport!(
            WARNING,
            PgSqlErrorCode::ERRCODE_WARNING,
            format!("no GIN index on {}.{}.{}", c.schema, c.table, c.column),
            format!(
                "Deleting from {}.{} will scan {}.{} on every row. \
                 Consider: CREATE INDEX ON {}.{} USING gin ({});",
                p.schema, p.table, c.schema, c.table, c.schema, c.table, c.column
            )
        );
    }

    name
}

fn truncate_ident(s: &str) -> String {
    // Postgres truncates identifiers at NAMEDATALEN-1 = 63 bytes.
    if s.len() <= 63 {
        s.to_string()
    } else {
        s.chars().take(63).collect()
    }
}

/// Remove a constraint and its triggers.
#[pg_extern(schema = "anyfk")]
fn drop_constraint(conname: &str) -> String {
    let (id, child_oid, parent_oid) = lookup(conname);

    Spi::connect_mut(|client| {
        client
            .update(
                "SELECT anyfk._drop_triggers($1,$2::oid::regclass,$3::oid::regclass)",
                Some(1),
                &[id.into(), child_oid.into(), parent_oid.into()],
            )
            .or_pg("dropping the triggers");
        client
            .update("SELECT anyfk._unregister($1)", Some(1), &[conname.into()])
            .or_pg("unregistering the constraint");
    });

    conname.to_string()
}

/// Elements of an array column that have no matching parent row, worst first.
///
/// Reads only; useful before adding a constraint, or after adding one with
/// `validate => false`.
#[allow(clippy::type_complexity)]
#[pg_extern(schema = "anyfk")]
fn violations(
    child: &str,
    parent: &str,
) -> TableIterator<
    'static,
    (
        name!(missing_element, Option<String>),
        name!(child_rows, Option<i64>),
    ),
> {
    let child_spec = match ParentRef::parse(child) {
        Ok(s) => s,
        Err(e) => error!("child: {e}"),
    };
    let parent_spec = match ParentRef::parse(parent) {
        Ok(s) => s,
        Err(e) => error!("parent: {e}"),
    };
    let c = resolve(&child_spec, "referencing table");
    let p = resolve(&parent_spec, "referenced table");

    if c.element_type.is_none() {
        error!(
            "column {} of {}.{} is {}, not an array",
            c.column, c.schema, c.table, c.type_name
        );
    }

    let rows = Spi::connect(|client| {
        client
            .select(&violations_sql(&c, &p), None, &[])
            .or_pg("listing violations")
            .map(|row| {
                (
                    row.get::<String>(1).unwrap_or_default(),
                    row.get::<i64>(2).unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
    });

    TableIterator::new(rows)
}
