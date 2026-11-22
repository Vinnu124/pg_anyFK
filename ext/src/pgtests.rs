//! In-Postgres tests.
//!
//! Each test runs in a transaction that is rolled back, so fixtures do not leak.
//! The concurrency test lives in `scripts/race_test.sh` instead, because proving
//! the parent row lock works needs two sessions at once.

use pgrx::prelude::*;

#[pg_schema]
pub mod tests {
    use pgrx::prelude::*;

    fn run(sql: &str) {
        Spi::run(sql).expect(sql);
    }

    /// `tags` 1..10 and a `posts` table with an array column, constraint installed.
    fn fixture() {
        run("CREATE TABLE tags (id bigint PRIMARY KEY, label text)");
        run("INSERT INTO tags SELECT g, 'tag'||g FROM generate_series(1,10) g");
        run("CREATE TABLE posts (id bigserial PRIMARY KEY, \
             tag_ids bigint[] NOT NULL DEFAULT '{}', note text)");
        run("CREATE INDEX ON posts USING gin (tag_ids)");
        run("SELECT anyfk.add('posts(tag_ids)', 'tags(id)')");
    }

    fn count(sql: &str) -> i64 {
        Spi::get_one::<i64>(sql).expect(sql).unwrap_or(-1)
    }

    // --------------------------------------------------------------------- basics

    #[pg_test]
    fn version_is_callable() {
        let v = Spi::get_one::<String>("SELECT anyfk.version()")
            .expect("version")
            .expect("null");
        assert!(v.starts_with("pg_anyfk "), "got {v}");
        assert!(v.contains("core "), "got {v}");
    }

    #[pg_test]
    fn add_records_the_constraint() {
        fixture();
        assert_eq!(
            count("SELECT count(*) FROM anyfk.constraints WHERE conname = 'posts_tag_ids_anyfk'"),
            1
        );
        let et = Spi::get_one::<String>("SELECT element_type FROM anyfk.constraints")
            .expect("element_type")
            .expect("null");
        assert_eq!(et, "bigint");
    }

    #[pg_test]
    fn add_installs_three_triggers() {
        fixture();
        assert_eq!(
            count(
                "SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'anyfk_%' AND NOT tgisinternal"
            ),
            3,
            "child check, parent delete/update, parent truncate"
        );
    }

    // -------------------------------------------------------------- child side

    #[pg_test]
    fn accepts_valid_elements() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1,2,3}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
    }

    #[pg_test(
        error = "insert or update on table \"posts\" violates foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn rejects_a_missing_element_on_insert() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1,999}')");
    }

    #[pg_test(
        error = "insert or update on table \"posts\" violates foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn rejects_a_missing_element_on_update() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1}')");
        run("UPDATE posts SET tag_ids = '{1,999}'");
    }

    #[pg_test]
    fn null_and_empty_arrays_are_allowed() {
        fixture();
        run("ALTER TABLE posts ALTER COLUMN tag_ids DROP NOT NULL");
        run("INSERT INTO posts (tag_ids) VALUES (NULL)");
        run("INSERT INTO posts (tag_ids) VALUES ('{}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 2);
    }

    #[pg_test]
    fn null_elements_are_skipped_like_match_simple() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1,NULL,2}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
    }

    #[pg_test]
    fn duplicate_elements_are_fine() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{3,3,3}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
    }

    #[pg_test]
    fn an_update_that_leaves_the_array_alone_is_allowed() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1}')");
        // Remove the parent behind the constraint's back, then touch another column.
        run("ALTER TABLE tags DISABLE TRIGGER ALL");
        run("DELETE FROM tags WHERE id = 1");
        run("ALTER TABLE tags ENABLE TRIGGER ALL");
        run("UPDATE posts SET note = 'edited'");
        assert_eq!(count("SELECT count(*) FROM posts WHERE note = 'edited'"), 1);
    }

    // ------------------------------------------------------------- parent side

    #[pg_test(
        error = "update or delete on table \"tags\" violates foreign key constraint \"posts_tag_ids_anyfk\" on table \"posts\""
    )]
    fn refuses_to_delete_a_referenced_parent() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{4}')");
        run("DELETE FROM tags WHERE id = 4");
    }

    #[pg_test]
    fn allows_deleting_an_unreferenced_parent() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{4}')");
        run("DELETE FROM tags WHERE id = 7");
        assert_eq!(count("SELECT count(*) FROM tags"), 9);
    }

    #[pg_test(
        error = "update or delete on table \"tags\" violates foreign key constraint \"posts_tag_ids_anyfk\" on table \"posts\""
    )]
    fn refuses_to_change_a_referenced_parent_key() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{4}')");
        run("UPDATE tags SET id = 400 WHERE id = 4");
    }

    #[pg_test]
    fn allows_updating_a_parent_column_that_is_not_the_key() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{4}')");
        run("UPDATE tags SET label = 'renamed' WHERE id = 4");
        assert_eq!(
            count("SELECT count(*) FROM tags WHERE label = 'renamed'"),
            1
        );
    }

    #[pg_test(
        error = "cannot truncate a table referenced in a foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn refuses_to_truncate_a_referenced_parent() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{4}')");
        run("TRUNCATE tags");
    }

    #[pg_test]
    fn allows_truncating_the_parent_when_nothing_references_it() {
        fixture();
        run("TRUNCATE tags");
        assert_eq!(count("SELECT count(*) FROM tags"), 0);
    }

    /// `format_type` prints the modifier for a column but not for an array's element
    /// type, which made this pair look mismatched. Types are compared by OID now.
    #[pg_test]
    fn a_type_modifier_does_not_break_the_type_check() {
        run("CREATE TABLE codes (c varchar(10) PRIMARY KEY)");
        run("CREATE TABLE uses (cs varchar(10)[])");
        run("SELECT anyfk.add('uses(cs)', 'codes(c)')");
        run("INSERT INTO codes VALUES ('ok')");
        run("INSERT INTO uses VALUES ('{ok}')");
        assert_eq!(count("SELECT count(*) FROM uses"), 1);
    }

    /// A native foreign key allows `TRUNCATE parent, child`, so the guard fires
    /// AFTER truncation: by then the child is empty and this form is legal, while
    /// truncating the parent alone still fails.
    #[pg_test]
    fn truncating_parent_and_child_together_is_allowed() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{4}')");
        run("TRUNCATE tags, posts");
        assert_eq!(count("SELECT count(*) FROM tags"), 0);
        assert_eq!(count("SELECT count(*) FROM posts"), 0);
    }

    // ------------------------------------------------------- add() validation

    #[pg_test(
        error = "column note of public.posts is text, not an array; use a native FOREIGN KEY instead"
    )]
    fn add_rejects_a_non_array_column() {
        fixture();
        run("SELECT anyfk.add('posts(note)', 'tags(id)')");
    }

    #[pg_test(
        error = "type mismatch: public.posts.int_ids has elements of type integer but public.tags.id is bigint"
    )]
    fn add_rejects_a_type_mismatch() {
        fixture();
        run("ALTER TABLE posts ADD COLUMN int_ids integer[]");
        run("SELECT anyfk.add('posts(int_ids)', 'tags(id)')");
    }

    #[pg_test(
        error = "there is no unique constraint matching the referenced column public.loose.k"
    )]
    fn add_rejects_a_parent_column_without_a_unique_index() {
        fixture();
        run("CREATE TABLE loose (k bigint)");
        run("SELECT anyfk.add('posts(tag_ids)', 'loose(k)')");
    }

    #[pg_test(error = "parent: reference target must name a column, as in 'tags(id)'")]
    fn add_rejects_a_parent_without_a_column() {
        fixture();
        run("SELECT anyfk.add('posts(tag_ids)', 'tags')");
    }

    #[pg_test(error = "referencing table \"nope\" does not exist, or has no column \"tag_ids\"")]
    fn add_rejects_a_missing_table() {
        run("SELECT anyfk.add('nope(tag_ids)', 'nope2(id)')");
    }

    #[pg_test(
        error = "unknown on_delete mode \"cascade\"; expected one of restrict, scrub, set_null"
    )]
    fn add_rejects_cascade() {
        fixture();
        run("ALTER TABLE posts ADD COLUMN more_ids bigint[]");
        run("SELECT anyfk.add('posts(more_ids)', 'tags(id)', on_delete => 'cascade')");
    }

    #[pg_test(error = "on_delete => 'set_null' is not implemented yet; use 'restrict' or 'scrub'")]
    fn add_rejects_modes_that_are_not_built_yet() {
        fixture();
        run("ALTER TABLE posts ADD COLUMN more_ids bigint[]");
        run("SELECT anyfk.add('posts(more_ids)', 'tags(id)', on_delete => 'set_null')");
    }

    // ----------------------------------------------------------------------- scrub

    /// Same as `fixture()` but with `on_delete => 'scrub'`.
    fn scrub_fixture() {
        run("CREATE TABLE tags (id bigint PRIMARY KEY, label text)");
        run("INSERT INTO tags SELECT g, 'tag'||g FROM generate_series(1,10) g");
        run("CREATE TABLE posts (id bigserial PRIMARY KEY, \
             tag_ids bigint[] NOT NULL DEFAULT '{}', note text)");
        run("CREATE INDEX ON posts USING gin (tag_ids)");
        run("SELECT anyfk.add('posts(tag_ids)', 'tags(id)', on_delete => 'scrub')");
    }

    #[pg_test]
    fn scrub_is_accepted_and_stored() {
        scrub_fixture();
        let m = Spi::get_one::<String>("SELECT on_delete FROM anyfk.constraints")
            .expect("on_delete")
            .expect("null");
        assert_eq!(m, "scrub");
    }

    #[pg_test]
    fn scrub_removes_the_key_and_allows_the_delete() {
        scrub_fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1,2,3}')");
        run("DELETE FROM tags WHERE id = 2");
        let a = Spi::get_one::<String>("SELECT tag_ids::text FROM posts")
            .expect("tag_ids")
            .expect("null");
        assert_eq!(a, "{1,3}");
        assert_eq!(count("SELECT count(*) FROM tags WHERE id = 2"), 0);
    }

    #[pg_test]
    fn scrub_keeps_the_row_rather_than_deleting_it() {
        scrub_fixture();
        run("INSERT INTO posts (tag_ids, note) VALUES ('{4}', 'keep me')");
        run("DELETE FROM tags WHERE id = 4");
        // The row survives with an empty array. scrub is not cascade.
        assert_eq!(
            count("SELECT count(*) FROM posts WHERE note = 'keep me'"),
            1
        );
        let a = Spi::get_one::<String>("SELECT tag_ids::text FROM posts")
            .expect("tag_ids")
            .expect("null");
        assert_eq!(a, "{}");
    }

    #[pg_test]
    fn scrub_removes_every_occurrence_and_touches_only_matching_rows() {
        scrub_fixture();
        run("INSERT INTO posts (tag_ids, note) VALUES ('{5,6,5}', 'dupes')");
        run("INSERT INTO posts (tag_ids, note) VALUES ('{7,8}', 'untouched')");
        run("DELETE FROM tags WHERE id = 5");
        let dupes = Spi::get_one::<String>("SELECT tag_ids::text FROM posts WHERE note = 'dupes'")
            .expect("tag_ids")
            .expect("null");
        assert_eq!(dupes, "{6}");
        let other =
            Spi::get_one::<String>("SELECT tag_ids::text FROM posts WHERE note = 'untouched'")
                .expect("tag_ids")
                .expect("null");
        assert_eq!(other, "{7,8}");
    }

    #[pg_test]
    fn scrub_leaves_null_arrays_alone() {
        scrub_fixture();
        run("ALTER TABLE posts ALTER COLUMN tag_ids DROP NOT NULL");
        run("INSERT INTO posts (tag_ids, note) VALUES (NULL, 'null row')");
        run("INSERT INTO posts (tag_ids, note) VALUES ('{9}', 'has nine')");
        run("DELETE FROM tags WHERE id = 9");
        assert_eq!(
            count("SELECT count(*) FROM posts WHERE tag_ids IS NULL AND note = 'null row'"),
            1
        );
    }

    #[pg_test]
    fn scrub_deleting_an_unreferenced_key_changes_nothing() {
        scrub_fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1,2}')");
        run("DELETE FROM tags WHERE id = 10");
        let a = Spi::get_one::<String>("SELECT tag_ids::text FROM posts")
            .expect("tag_ids")
            .expect("null");
        assert_eq!(a, "{1,2}");
    }

    #[pg_test(
        error = "insert or update on table \"posts\" violates foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn scrub_still_rejects_inserting_a_missing_element() {
        scrub_fixture();
        // scrub governs the parent side only; the child check is unchanged.
        run("INSERT INTO posts (tag_ids) VALUES ('{999}')");
    }

    // on_delete says nothing about updates, so an UPDATE of a referenced key is
    // refused whatever the mode.
    #[pg_test(
        error = "update or delete on table \"tags\" violates foreign key constraint \"posts_tag_ids_anyfk\" on table \"posts\""
    )]
    fn scrub_does_not_apply_to_updating_a_key() {
        scrub_fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{3}')");
        run("UPDATE tags SET id = 99 WHERE id = 3");
    }

    #[pg_test]
    fn scrub_handles_many_rows_in_one_delete() {
        scrub_fixture();
        run("INSERT INTO posts (tag_ids) SELECT '{1,2}'::bigint[] FROM generate_series(1,200)");
        run("DELETE FROM tags WHERE id = 1");
        assert_eq!(
            count("SELECT count(*) FROM posts WHERE tag_ids = '{2}'"),
            200
        );
    }

    // The scrub UPDATE runs as the deleting role, so that role needs UPDATE on the
    // referencing table. We do not elevate for it.
    #[pg_test(error = "permission denied for table posts")]
    fn scrub_needs_update_on_the_referencing_table() {
        scrub_fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1}')");
        run("CREATE ROLE anyfk_no_update");
        run("GRANT SELECT, DELETE ON tags TO anyfk_no_update");
        run("GRANT SELECT ON posts TO anyfk_no_update");
        run("SET ROLE anyfk_no_update");
        run("DELETE FROM tags WHERE id = 1");
    }

    #[pg_test]
    fn restrict_is_still_the_default() {
        fixture();
        let m = Spi::get_one::<String>("SELECT on_delete FROM anyfk.constraints")
            .expect("on_delete")
            .expect("null");
        assert_eq!(m, "restrict");
    }

    #[pg_test(
        error = "update or delete on table \"tags\" violates foreign key constraint \"posts_tag_ids_anyfk\" on table \"posts\""
    )]
    fn restrict_still_refuses_the_delete() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1}')");
        run("DELETE FROM tags WHERE id = 1");
    }

    #[pg_test(error = "existing rows in public.posts violate the constraint")]
    fn add_rejects_existing_violations() {
        run("CREATE TABLE tags (id bigint PRIMARY KEY)");
        run("INSERT INTO tags VALUES (1)");
        run("CREATE TABLE posts (id bigserial PRIMARY KEY, tag_ids bigint[])");
        run("INSERT INTO posts (tag_ids) VALUES ('{1,42}')");
        run("SELECT anyfk.add('posts(tag_ids)', 'tags(id)')");
    }

    #[pg_test]
    fn add_can_skip_validation() {
        run("CREATE TABLE tags (id bigint PRIMARY KEY)");
        run("INSERT INTO tags VALUES (1)");
        run("CREATE TABLE posts (id bigserial PRIMARY KEY, tag_ids bigint[])");
        run("INSERT INTO posts (tag_ids) VALUES ('{1,42}')");
        run("SELECT anyfk.add('posts(tag_ids)', 'tags(id)', validate => false)");
        // Pre-existing bad data survives; new bad data does not.
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
        assert_eq!(count("SELECT count(*) FROM anyfk.constraints"), 1);
    }

    #[pg_test(error = "a column cannot reference itself")]
    fn add_rejects_a_self_reference() {
        run("CREATE TABLE weird (ids bigint[] PRIMARY KEY)");
        run("SELECT anyfk.add('weird(ids)', 'weird(ids)')");
    }

    // ------------------------------------------------------ violations & drop

    #[pg_test]
    fn violations_lists_missing_elements_worst_first() {
        run("CREATE TABLE tags (id bigint PRIMARY KEY)");
        run("INSERT INTO tags VALUES (1)");
        run("CREATE TABLE posts (id bigserial PRIMARY KEY, tag_ids bigint[])");
        run("INSERT INTO posts (tag_ids) VALUES ('{1,42}'), ('{42}'), ('{99}')");

        let rows = count("SELECT count(*) FROM anyfk.violations('posts(tag_ids)','tags(id)')");
        assert_eq!(rows, 2, "elements 42 and 99");

        let worst = Spi::get_one::<String>(
            "SELECT missing_element FROM anyfk.violations('posts(tag_ids)','tags(id)') LIMIT 1",
        )
        .expect("violations")
        .expect("null");
        assert_eq!(worst, "42", "42 appears in two rows, so it sorts first");
    }

    #[pg_test]
    fn violations_is_empty_when_data_is_clean() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1,2}')");
        assert_eq!(
            count("SELECT count(*) FROM anyfk.violations('posts(tag_ids)','tags(id)')"),
            0
        );
    }

    #[pg_test]
    fn drop_removes_the_catalog_row_and_the_triggers() {
        fixture();
        run("SELECT anyfk.drop_constraint('posts_tag_ids_anyfk')");
        assert_eq!(count("SELECT count(*) FROM anyfk.constraints"), 0);
        assert_eq!(
            count(
                "SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'anyfk_%' AND NOT tgisinternal"
            ),
            0
        );
        // No longer enforced.
        run("INSERT INTO posts (tag_ids) VALUES ('{999}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
    }

    #[pg_test(error = "constraint \"nope\" does not exist")]
    fn drop_rejects_an_unknown_constraint() {
        fixture();
        run("SELECT anyfk.drop_constraint('nope')");
    }

    #[pg_test(error = "constraint \"posts_tag_ids_anyfk\" already exists")]
    fn add_rejects_a_duplicate_name() {
        fixture();
        run("ALTER TABLE posts ADD COLUMN more_ids bigint[]");
        run("SELECT anyfk.add('posts(more_ids)', 'tags(id)', \
             conname => 'posts_tag_ids_anyfk')");
    }

    // -------------------------------------------------------------- renames
    //
    // The catalog stores each table as a `regclass`, which holds an OID, and the
    // triggers read the current name from it on every call. Renaming a table used
    // to make every write to the child fail, valid or not.

    #[pg_test]
    fn renaming_the_parent_table_keeps_the_constraint_working() {
        fixture();
        run("ALTER TABLE tags RENAME TO labels");
        run("INSERT INTO posts (tag_ids) VALUES ('{1}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
    }

    #[pg_test(
        error = "insert or update on table \"posts\" violates foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn a_bad_element_is_still_rejected_after_renaming_the_parent() {
        fixture();
        run("ALTER TABLE tags RENAME TO labels");
        run("INSERT INTO posts (tag_ids) VALUES ('{999}')");
    }

    #[pg_test(
        error = "insert or update on table \"articles\" violates foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn a_bad_element_is_still_rejected_after_renaming_the_child() {
        fixture();
        run("ALTER TABLE posts RENAME TO articles");
        run("INSERT INTO articles (tag_ids) VALUES ('{999}')");
    }

    #[pg_test(
        error = "insert or update on table \"posts\" violates foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn a_bad_element_is_still_rejected_after_moving_the_parent_schema() {
        fixture();
        run("CREATE SCHEMA other");
        run("ALTER TABLE tags SET SCHEMA other");
        run("INSERT INTO posts (tag_ids) VALUES ('{999}')");
    }

    #[pg_test]
    fn the_catalog_reports_current_names_after_a_rename() {
        fixture();
        run("ALTER TABLE tags RENAME TO labels");
        let p = Spi::get_one::<String>("SELECT parent_rel::text FROM anyfk.constraints")
            .expect("parent_rel")
            .expect("null");
        assert_eq!(p, "labels");
    }

    #[pg_test]
    fn renaming_the_referencing_column_keeps_the_constraint_working() {
        fixture();
        run("ALTER TABLE posts RENAME COLUMN tag_ids TO tarr");
        run("INSERT INTO posts (tarr) VALUES ('{1}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
        let col = Spi::get_one::<String>("SELECT child_column::text FROM anyfk.constraints")
            .expect("child_column")
            .expect("null");
        assert_eq!(col, "tarr", "the catalog must follow the rename");
    }

    #[pg_test(
        error = "insert or update on table \"posts\" violates foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn a_bad_element_is_still_rejected_after_renaming_the_referencing_column() {
        fixture();
        run("ALTER TABLE posts RENAME COLUMN tag_ids TO tarr");
        run("INSERT INTO posts (tarr) VALUES ('{999}')");
    }

    #[pg_test(
        error = "insert or update on table \"posts\" violates foreign key constraint \"posts_tag_ids_anyfk\""
    )]
    fn a_bad_element_is_still_rejected_after_renaming_the_referenced_column() {
        fixture();
        run("ALTER TABLE tags RENAME COLUMN id TO tag_id");
        run("INSERT INTO posts (tag_ids) VALUES ('{999}')");
    }

    #[pg_test(
        error = "update or delete on table \"tags\" violates foreign key constraint \"posts_tag_ids_anyfk\" on table \"posts\""
    )]
    fn the_parent_side_still_restricts_after_renaming_both_columns() {
        fixture();
        run("INSERT INTO posts (tag_ids) VALUES ('{1}')");
        run("ALTER TABLE posts RENAME COLUMN tag_ids TO tarr");
        run("ALTER TABLE tags RENAME COLUMN id TO tag_id");
        run("DELETE FROM tags WHERE tag_id = 1");
    }

    /// Adding an unrelated column must not disturb the stored names or attnums.
    #[pg_test]
    fn an_unrelated_alter_table_leaves_the_catalog_alone() {
        fixture();
        run("ALTER TABLE posts ADD COLUMN body text");
        let (col, num) = Spi::get_two::<String, i16>(
            "SELECT child_column::text, child_attnum FROM anyfk.constraints",
        )
        .expect("catalog row");
        assert_eq!(col.as_deref(), Some("tag_ids"));
        assert_eq!(num, Some(2));
    }

    // ------------------------------------------------------- catalog self-cleaning

    #[pg_test]
    fn dropping_the_child_table_removes_the_constraint() {
        fixture();
        run("DROP TABLE posts");
        assert_eq!(count("SELECT count(*) FROM anyfk.constraints"), 0);
    }

    #[pg_test]
    fn dropping_the_parent_table_removes_the_constraint_and_the_child_trigger() {
        fixture();
        run("DROP TABLE tags");
        assert_eq!(count("SELECT count(*) FROM anyfk.constraints"), 0);
        assert_eq!(
            count(
                "SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'anyfk_%' AND NOT tgisinternal"
            ),
            0,
            "the child trigger would otherwise fail on every write"
        );
        // Writes must work again rather than erroring on a missing parent.
        run("INSERT INTO posts (tag_ids) VALUES ('{999}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
    }

    /// The child trigger is declared `UPDATE OF tag_ids`, which makes Postgres
    /// treat the column as a dependency and refuse a bare drop. That is the same
    /// protection a native constraint gives, so we keep it rather than work around.
    #[pg_test(
        error = "cannot drop column tag_ids of table posts because other objects depend on it"
    )]
    fn dropping_the_array_column_is_refused() {
        fixture();
        run("ALTER TABLE posts DROP COLUMN tag_ids");
    }

    #[pg_test]
    fn dropping_the_array_column_with_cascade_cleans_up_fully() {
        fixture();
        run("ALTER TABLE posts DROP COLUMN tag_ids CASCADE");
        assert_eq!(count("SELECT count(*) FROM anyfk.constraints"), 0);
        assert_eq!(
            count(
                "SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'anyfk_%' AND NOT tgisinternal"
            ),
            0,
            "the parent triggers would otherwise reference a column that is gone"
        );
        // And the parent is usable again.
        run("DELETE FROM tags WHERE id = 1");
        assert_eq!(count("SELECT count(*) FROM tags"), 9);
    }

    #[pg_test]
    fn a_constraint_can_be_re_added_after_the_table_is_recreated() {
        fixture();
        run("DROP TABLE posts");
        run("CREATE TABLE posts (id bigserial PRIMARY KEY, tag_ids bigint[])");
        // Would fail with 'already exists' if the catalog row had leaked.
        run("SELECT anyfk.add('posts(tag_ids)', 'tags(id)')");
        assert_eq!(count("SELECT count(*) FROM anyfk.constraints"), 1);
    }

    // ------------------------------------------------------------- deferrable

    #[pg_test]
    fn a_deferrable_constraint_allows_child_before_parent() {
        run("CREATE TABLE tags (id bigint PRIMARY KEY)");
        run("CREATE TABLE posts (id bigserial PRIMARY KEY, tag_ids bigint[])");
        run("SELECT anyfk.add('posts(tag_ids)', 'tags(id)', is_deferrable => true)");
        // Inside one transaction the order does not matter.
        run("INSERT INTO posts (tag_ids) VALUES ('{5}')");
        run("INSERT INTO tags VALUES (5)");
        run("SET CONSTRAINTS ALL IMMEDIATE");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
    }

    #[pg_test]
    fn multiple_constraints_on_one_table_coexist() {
        fixture();
        run("CREATE TABLE authors (id bigint PRIMARY KEY)");
        run("INSERT INTO authors VALUES (1)");
        run("ALTER TABLE posts ADD COLUMN author_ids bigint[]");
        run("SELECT anyfk.add('posts(author_ids)', 'authors(id)')");
        assert_eq!(count("SELECT count(*) FROM anyfk.constraints"), 2);
        run("INSERT INTO posts (tag_ids, author_ids) VALUES ('{1}', '{1}')");
        assert_eq!(count("SELECT count(*) FROM posts"), 1);
    }
}
