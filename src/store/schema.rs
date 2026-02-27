use rusqlite::Connection;

/// Schema DDL run on open.
pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS folders (
    account_id TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    mailbox_hash INTEGER NOT NULL,
    unread_count INTEGER DEFAULT 0,
    total_count INTEGER DEFAULT 0,
    PRIMARY KEY (account_id, path),
    UNIQUE (account_id, mailbox_hash)
);

CREATE TABLE IF NOT EXISTS messages (
    account_id TEXT NOT NULL,
    envelope_hash INTEGER NOT NULL,
    mailbox_hash INTEGER NOT NULL,
    subject TEXT,
    sender TEXT,
    date TEXT,
    timestamp INTEGER NOT NULL DEFAULT 0,
    is_read INTEGER DEFAULT 0,
    is_starred INTEGER DEFAULT 0,
    has_attachments INTEGER DEFAULT 0,
    thread_id INTEGER,
    body_rendered TEXT,
    PRIMARY KEY (account_id, envelope_hash),
    FOREIGN KEY (account_id, mailbox_hash) REFERENCES folders(account_id, mailbox_hash)
);

CREATE INDEX IF NOT EXISTS idx_messages_mailbox
    ON messages(account_id, mailbox_hash, timestamp DESC);

CREATE TABLE IF NOT EXISTS attachments (
    account_id TEXT NOT NULL,
    envelope_hash INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    filename TEXT NOT NULL DEFAULT 'unnamed',
    mime_type TEXT NOT NULL DEFAULT 'application/octet-stream',
    data BLOB NOT NULL,
    PRIMARY KEY (account_id, envelope_hash, idx),
    FOREIGN KEY (account_id, envelope_hash) REFERENCES messages(account_id, envelope_hash) ON DELETE CASCADE
);
";

/// Run forward-only migrations. Each ALTER is idempotent (ignores "duplicate column" errors).
pub(super) fn run_migrations(conn: &Connection) {
    let alters = [
        "ALTER TABLE messages ADD COLUMN flags_server INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN flags_local INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN pending_op TEXT",
        "ALTER TABLE messages ADD COLUMN message_id TEXT",
        "ALTER TABLE messages ADD COLUMN in_reply_to TEXT",
        "ALTER TABLE messages ADD COLUMN thread_depth INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN body_markdown TEXT",
        "ALTER TABLE messages ADD COLUMN reply_to TEXT",
        "ALTER TABLE messages ADD COLUMN recipient TEXT",
        // Multi-account support
        "ALTER TABLE folders ADD COLUMN account_id TEXT DEFAULT ''",
        "ALTER TABLE messages ADD COLUMN account_id TEXT DEFAULT ''",
    ];
    for sql in &alters {
        // "duplicate column name" is the expected error when already migrated
        if let Err(e) = conn.execute(sql, []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                log::warn!("Migration failed ({}): {}", sql, msg);
            }
        }
    }

    if let Err(e) = migrate_to_account_scoped_primary_keys(conn) {
        log::warn!("Primary-key migration failed: {}", e);
    }

    // Indexes (idempotent via IF NOT EXISTS)
    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_messages_message_id ON messages(message_id)",
        "CREATE INDEX IF NOT EXISTS idx_folders_account ON folders(account_id)",
        "CREATE INDEX IF NOT EXISTS idx_messages_account_mailbox ON messages(account_id, mailbox_hash, timestamp DESC)",
    ];
    for sql in &indexes {
        if let Err(e) = conn.execute(sql, []) {
            log::warn!("Index creation failed: {}", e);
        }
    }

    // FTS5 full-text search index (external content, keyed to messages rowid).
    // Column names MUST match the content table for rebuild to work.
    // Drop stale FTS objects from earlier schema that used wrong column name ('body').
    for stale in &[
        "DROP TRIGGER IF EXISTS messages_fts_ai",
        "DROP TRIGGER IF EXISTS messages_fts_ad",
        "DROP TRIGGER IF EXISTS messages_fts_au",
        "DROP TABLE IF EXISTS message_fts",
    ] {
        let _ = conn.execute_batch(stale);
    }

    let fts_ddl = [
        "CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
            subject,
            sender,
            body_rendered,
            content='messages',
            content_rowid='rowid'
        )",
        // Auto-sync triggers
        "CREATE TRIGGER IF NOT EXISTS messages_fts_ai AFTER INSERT ON messages BEGIN
          INSERT INTO message_fts(rowid, subject, sender, body_rendered)
          VALUES (new.rowid, new.subject, new.sender, new.body_rendered);
        END",
        "CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages BEGIN
          INSERT INTO message_fts(message_fts, rowid, subject, sender, body_rendered)
          VALUES('delete', old.rowid, old.subject, old.sender, old.body_rendered);
        END",
        "CREATE TRIGGER IF NOT EXISTS messages_fts_au AFTER UPDATE ON messages BEGIN
          INSERT INTO message_fts(message_fts, rowid, subject, sender, body_rendered)
          VALUES('delete', old.rowid, old.subject, old.sender, old.body_rendered);
          INSERT INTO message_fts(rowid, subject, sender, body_rendered)
          VALUES (new.rowid, new.subject, new.sender, new.body_rendered);
        END",
    ];
    for ddl in &fts_ddl {
        if let Err(e) = conn.execute_batch(ddl) {
            log::warn!(
                "FTS5 migration failed ({}): {}",
                ddl.chars().take(60).collect::<String>(),
                e
            );
        }
    }

    // Rebuild FTS index from existing content (idempotent, fast if current)
    if let Err(e) = conn.execute("INSERT INTO message_fts(message_fts) VALUES('rebuild')", []) {
        log::warn!("FTS5 rebuild failed: {}", e);
    }
}

fn migrate_to_account_scoped_primary_keys(conn: &Connection) -> Result<(), String> {
    let messages_is_scoped =
        table_pk_columns(conn, "messages") == vec!["account_id", "envelope_hash"];
    let folders_is_scoped = table_pk_columns(conn, "folders") == vec!["account_id", "path"];
    let attachments_is_scoped =
        table_pk_columns(conn, "attachments") == vec!["account_id", "envelope_hash", "idx"];
    if messages_is_scoped && folders_is_scoped && attachments_is_scoped {
        return Ok(());
    }

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("migration tx error: {e}"))?;

    tx.execute_batch(
        "
        DROP TRIGGER IF EXISTS messages_fts_ai;
        DROP TRIGGER IF EXISTS messages_fts_ad;
        DROP TRIGGER IF EXISTS messages_fts_au;
        DROP TABLE IF EXISTS message_fts;

        CREATE TABLE folders_v2 (
            account_id TEXT NOT NULL,
            path TEXT NOT NULL,
            name TEXT NOT NULL,
            mailbox_hash INTEGER NOT NULL,
            unread_count INTEGER DEFAULT 0,
            total_count INTEGER DEFAULT 0,
            PRIMARY KEY (account_id, path),
            UNIQUE (account_id, mailbox_hash)
        );

        CREATE TABLE messages_v2 (
            account_id TEXT NOT NULL,
            envelope_hash INTEGER NOT NULL,
            mailbox_hash INTEGER NOT NULL,
            subject TEXT,
            sender TEXT,
            date TEXT,
            timestamp INTEGER NOT NULL DEFAULT 0,
            is_read INTEGER DEFAULT 0,
            is_starred INTEGER DEFAULT 0,
            has_attachments INTEGER DEFAULT 0,
            thread_id INTEGER,
            body_rendered TEXT,
            flags_server INTEGER DEFAULT 0,
            flags_local INTEGER DEFAULT 0,
            pending_op TEXT,
            message_id TEXT,
            in_reply_to TEXT,
            thread_depth INTEGER DEFAULT 0,
            body_markdown TEXT,
            reply_to TEXT,
            recipient TEXT,
            PRIMARY KEY (account_id, envelope_hash),
            FOREIGN KEY (account_id, mailbox_hash) REFERENCES folders_v2(account_id, mailbox_hash)
        );

        CREATE TABLE attachments_v2 (
            account_id TEXT NOT NULL,
            envelope_hash INTEGER NOT NULL,
            idx INTEGER NOT NULL,
            filename TEXT NOT NULL DEFAULT 'unnamed',
            mime_type TEXT NOT NULL DEFAULT 'application/octet-stream',
            data BLOB NOT NULL,
            PRIMARY KEY (account_id, envelope_hash, idx),
            FOREIGN KEY (account_id, envelope_hash) REFERENCES messages_v2(account_id, envelope_hash) ON DELETE CASCADE
        );
        ",
    )
    .map_err(|e| format!("create v2 tables error: {e}"))?;

    tx.execute_batch(
        "
        INSERT OR REPLACE INTO folders_v2 (account_id, path, name, mailbox_hash, unread_count, total_count)
        SELECT COALESCE(account_id, ''), path, name, mailbox_hash, unread_count, total_count
        FROM folders;

        INSERT OR REPLACE INTO messages_v2 (
            account_id, envelope_hash, mailbox_hash, subject, sender, date, timestamp,
            is_read, is_starred, has_attachments, thread_id, body_rendered, flags_server,
            flags_local, pending_op, message_id, in_reply_to, thread_depth, body_markdown,
            reply_to, recipient
        )
        SELECT
            COALESCE(account_id, ''), envelope_hash, mailbox_hash, subject, sender, date, timestamp,
            is_read, is_starred, has_attachments, thread_id, body_rendered, COALESCE(flags_server, 0),
            COALESCE(flags_local, 0), pending_op, message_id, in_reply_to, COALESCE(thread_depth, 0),
            body_markdown, reply_to, recipient
        FROM messages;

        INSERT OR REPLACE INTO attachments_v2 (account_id, envelope_hash, idx, filename, mime_type, data)
        SELECT COALESCE(m.account_id, ''), a.envelope_hash, a.idx, a.filename, a.mime_type, a.data
        FROM attachments a
        LEFT JOIN messages m ON m.envelope_hash = a.envelope_hash;

        DROP TABLE attachments;
        DROP TABLE messages;
        DROP TABLE folders;

        ALTER TABLE folders_v2 RENAME TO folders;
        ALTER TABLE messages_v2 RENAME TO messages;
        ALTER TABLE attachments_v2 RENAME TO attachments;
        ",
    )
    .map_err(|e| format!("copy/swap v2 tables error: {e}"))?;

    tx.commit()
        .map_err(|e| format!("migration commit error: {e}"))?;
    Ok(())
}

fn table_pk_columns(conn: &Connection, table: &str) -> Vec<&'static str> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    let mut columns: Vec<(i64, String)> = Vec::new();
    let rows = match stmt.query_map([], |row| {
        let name: String = row.get(1)?;
        let pk: i64 = row.get(5)?;
        Ok((pk, name))
    }) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };

    for row in rows.flatten() {
        if row.0 > 0 {
            columns.push(row);
        }
    }
    columns.sort_by_key(|(pk, _)| *pk);

    columns
        .into_iter()
        .map(|(_, name)| match name.as_str() {
            "account_id" => "account_id",
            "envelope_hash" => "envelope_hash",
            "path" => "path",
            "idx" => "idx",
            _ => "",
        })
        .filter(|name| !name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::run_migrations;

    #[test]
    fn migrates_legacy_tables_to_account_scoped_keys() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "
            CREATE TABLE folders (
                path TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                mailbox_hash INTEGER NOT NULL UNIQUE,
                unread_count INTEGER DEFAULT 0,
                total_count INTEGER DEFAULT 0
            );

            CREATE TABLE messages (
                envelope_hash INTEGER PRIMARY KEY,
                mailbox_hash INTEGER NOT NULL,
                subject TEXT,
                sender TEXT,
                date TEXT,
                timestamp INTEGER NOT NULL DEFAULT 0,
                is_read INTEGER DEFAULT 0,
                is_starred INTEGER DEFAULT 0,
                has_attachments INTEGER DEFAULT 0,
                thread_id INTEGER,
                body_rendered TEXT
            );

            CREATE TABLE attachments (
                envelope_hash INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                filename TEXT NOT NULL DEFAULT 'unnamed',
                mime_type TEXT NOT NULL DEFAULT 'application/octet-stream',
                data BLOB NOT NULL,
                PRIMARY KEY (envelope_hash, idx)
            );

            INSERT INTO folders (path, name, mailbox_hash, unread_count, total_count)
            VALUES ('INBOX', 'INBOX', 1, 2, 3);
            INSERT INTO messages (envelope_hash, mailbox_hash, subject, sender, date, timestamp, body_rendered)
            VALUES (10, 1, 'subj', 'from', 'date', 42, 'body');
            INSERT INTO attachments (envelope_hash, idx, filename, mime_type, data)
            VALUES (10, 0, 'file.txt', 'text/plain', X'6869');
            ",
        )
        .expect("create legacy schema");

        run_migrations(&conn);

        let subject: String = conn
            .query_row(
                "SELECT subject FROM messages WHERE account_id = '' AND envelope_hash = 10",
                [],
                |row| row.get(0),
            )
            .expect("migrated message exists");
        assert_eq!(subject, "subj");

        let attachment_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM attachments WHERE account_id = '' AND envelope_hash = 10",
                [],
                |row| row.get(0),
            )
            .expect("count attachments");
        assert_eq!(attachment_count, 1);

        conn.execute(
            "INSERT INTO folders (account_id, path, name, mailbox_hash, unread_count, total_count)
             VALUES ('a', 'INBOX', 'INBOX', 1, 0, 0)",
            [],
        )
        .expect("insert account a");
        conn.execute(
            "INSERT INTO folders (account_id, path, name, mailbox_hash, unread_count, total_count)
             VALUES ('b', 'INBOX', 'INBOX', 1, 0, 0)",
            [],
        )
        .expect("insert account b");
    }

    #[test]
    fn migration_rebuilds_fts_and_recreates_triggers() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "
            CREATE TABLE folders (
                path TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                mailbox_hash INTEGER NOT NULL UNIQUE,
                unread_count INTEGER DEFAULT 0,
                total_count INTEGER DEFAULT 0
            );

            CREATE TABLE messages (
                envelope_hash INTEGER PRIMARY KEY,
                mailbox_hash INTEGER NOT NULL,
                subject TEXT,
                sender TEXT,
                date TEXT,
                timestamp INTEGER NOT NULL DEFAULT 0,
                is_read INTEGER DEFAULT 0,
                is_starred INTEGER DEFAULT 0,
                has_attachments INTEGER DEFAULT 0,
                thread_id INTEGER,
                body_rendered TEXT
            );

            CREATE TABLE attachments (
                envelope_hash INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                filename TEXT NOT NULL DEFAULT 'unnamed',
                mime_type TEXT NOT NULL DEFAULT 'application/octet-stream',
                data BLOB NOT NULL,
                PRIMARY KEY (envelope_hash, idx)
            );

            INSERT INTO folders (path, name, mailbox_hash, unread_count, total_count)
            VALUES ('INBOX', 'INBOX', 1, 0, 1);
            INSERT INTO messages (
                envelope_hash, mailbox_hash, subject, sender, date, timestamp, body_rendered
            )
            VALUES (
                100, 1, 'premigrationneedle', 'legacy@example.com',
                '2026-01-01', 1000, 'legacy body'
            );
            ",
        )
        .expect("create legacy schema and seed data");

        run_migrations(&conn);

        // Rebuild should index rows that existed before migration.
        let pre_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_fts WHERE message_fts MATCH 'premigrationneedle'",
                [],
                |row| row.get(0),
            )
            .expect("query pre-migration fts");
        assert_eq!(pre_hits, 1);

        // Triggers should index rows inserted after migration without another rebuild.
        conn.execute(
            "INSERT INTO messages (
                account_id, envelope_hash, mailbox_hash, subject, sender, date, timestamp, body_rendered
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "",
                101_i64,
                1_i64,
                "postmigrationtriggerhit",
                "new@example.com",
                "2026-01-02",
                1001_i64,
                "new body",
            ],
        )
        .expect("insert post-migration row");

        let post_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_fts WHERE message_fts MATCH 'postmigrationtriggerhit'",
                [],
                |row| row.get(0),
            )
            .expect("query post-migration fts");
        assert_eq!(post_hits, 1);
    }
}
