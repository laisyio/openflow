use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

#[derive(Serialize, Clone)]
pub struct Transcription {
    pub id: String,
    pub raw_text: String,
    pub formatted_text: Option<String>,
    pub provider: String,
    pub duration_ms: Option<i64>,
    pub context_type: Option<String>,
    pub window_title: Option<String>,
    pub language: Option<String>,
    pub created_at: String,
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn new(app_dir: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&app_dir)
            .map_err(|e| format!("Failed to create app dir: {}", e))?;

        let db_path = app_dir.join("openflow.db");
        let conn =
            Connection::open(&db_path).map_err(|e| format!("Failed to open database: {}", e))?;

        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| format!("Failed to configure database: {}", e))?;

        // Deleting a row does not remove its text from the file. SQLite marks
        // the space free and leaves the bytes where they were, so a dictation
        // the user deleted -- or every dictation, after Clear history -- is
        // still readable in `openflow.db` afterwards, and stays readable after
        // the app quits. `secure_delete` overwrites freed content instead.
        //
        // Per connection and not stored in the file, so this has to run on
        // every open rather than once at creation.
        conn.execute_batch("PRAGMA secure_delete = ON")
            .map_err(|e| format!("Failed to configure database: {}", e))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS transcriptions (
                id TEXT PRIMARY KEY,
                raw_text TEXT NOT NULL,
                formatted_text TEXT,
                provider TEXT NOT NULL,
                duration_ms INTEGER,
                context_type TEXT,
                window_title TEXT,
                language TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .map_err(|e| format!("Migration failed: {}", e))?;

        Self::scrub_what_earlier_builds_left(&conn);

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Key recording that the one-time scrub below has already run.
    const SCRUBBED: &'static str = "history_file_scrubbed";

    /// Remove text that was deleted before `secure_delete` was turned on.
    ///
    /// The pragma above only governs deletes from here on. Every database this
    /// app has already written still holds the text of every transcription its
    /// user has ever deleted, and turning the pragma on does not touch it --
    /// only rewriting the file does. `VACUUM` is that rewrite.
    ///
    /// Once, not on every open: after this the freed pages are already
    /// overwritten as they are released, so there is nothing left for a second
    /// `VACUUM` to find, and it would be an unbounded rewrite at every launch.
    ///
    /// Best effort, and deliberately not a `?`. `VACUUM` needs the file to
    /// itself, and both builds of this app share one database -- refusing to
    /// open a database that is otherwise fine would be a worse outcome than
    /// scrubbing it on the next launch instead. A failure leaves the marker
    /// unset, which is what makes "next launch" true.
    fn scrub_what_earlier_builds_left(conn: &Connection) {
        let done = conn.query_row(
            "SELECT COUNT(*) FROM settings WHERE key = ?1",
            params![Self::SCRUBBED],
            |row| row.get::<_, i64>(0),
        );
        if !matches!(done, Ok(0)) {
            return;
        }
        if conn.execute_batch("VACUUM").is_ok() {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, '1')",
                params![Self::SCRUBBED],
            );
        }
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|_| "Database lock is poisoned".to_string())
    }

    pub fn save_transcription(&self, t: &Transcription) -> Result<(), String> {
        self.connection()?.execute(
            "INSERT OR REPLACE INTO transcriptions (id, raw_text, formatted_text, provider, duration_ms, context_type, window_title, language, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![t.id, t.raw_text, t.formatted_text, t.provider, t.duration_ms, t.context_type, t.window_title, t.language, t.created_at],
        ).map_err(|e| format!("Save failed: {}", e))?;
        Ok(())
    }

    pub fn get_history(&self, limit: usize) -> Result<Vec<Transcription>, String> {
        let limit = limit.min(500);
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, raw_text, formatted_text, provider, duration_ms, context_type, window_title, language, created_at
             FROM transcriptions ORDER BY created_at DESC LIMIT ?1"
        ).map_err(|e| format!("Query failed: {}", e))?;

        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(Transcription {
                    id: row.get(0)?,
                    raw_text: row.get(1)?,
                    formatted_text: row.get(2)?,
                    provider: row.get(3)?,
                    duration_ms: row.get(4)?,
                    context_type: row.get(5)?,
                    window_title: row.get(6)?,
                    language: row.get(7)?,
                    created_at: row.get(8)?,
                })
            })
            .map_err(|e| format!("Query map failed: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    pub fn search_history(&self, query: &str, limit: usize) -> Result<Vec<Transcription>, String> {
        let limit = limit.min(500);
        let pattern = format!("%{}%", query);
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, raw_text, formatted_text, provider, duration_ms, context_type, window_title, language, created_at
             FROM transcriptions WHERE raw_text LIKE ?1 OR formatted_text LIKE ?1
             ORDER BY created_at DESC LIMIT ?2"
        ).map_err(|e| format!("Search failed: {}", e))?;

        let rows = stmt
            .query_map(params![pattern, limit as i64], |row| {
                Ok(Transcription {
                    id: row.get(0)?,
                    raw_text: row.get(1)?,
                    formatted_text: row.get(2)?,
                    provider: row.get(3)?,
                    duration_ms: row.get(4)?,
                    context_type: row.get(5)?,
                    window_title: row.get(6)?,
                    language: row.get(7)?,
                    created_at: row.get(8)?,
                })
            })
            .map_err(|e| format!("Search map failed: {}", e))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    /// Fetch one row by id. The tray menu needs this so it can key entries by
    /// identity instead of by their position in a list that keeps changing.
    pub fn get_transcription(&self, id: &str) -> Result<Option<Transcription>, String> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, raw_text, formatted_text, provider, duration_ms, context_type, window_title, language, created_at
                 FROM transcriptions WHERE id = ?1",
            )
            .map_err(|e| format!("Query failed: {}", e))?;

        let mut rows = stmt
            .query_map(params![id], |row| {
                Ok(Transcription {
                    id: row.get(0)?,
                    raw_text: row.get(1)?,
                    formatted_text: row.get(2)?,
                    provider: row.get(3)?,
                    duration_ms: row.get(4)?,
                    context_type: row.get(5)?,
                    window_title: row.get(6)?,
                    language: row.get(7)?,
                    created_at: row.get(8)?,
                })
            })
            .map_err(|e| format!("Query map failed: {}", e))?;

        match rows.next() {
            Some(row) => Ok(Some(row.map_err(|e| format!("Row error: {}", e))?)),
            None => Ok(None),
        }
    }

    // ── Privacy controls ──────────────────────────────────
    // Dictation captures whatever the user says out loud: passwords, medical
    // details, private conversation. Storing all of it with no way to remove
    // any of it is the largest privacy gap the app can have.

    pub fn delete_transcription(&self, id: &str) -> Result<(), String> {
        let conn = self.connection()?;
        conn.execute("DELETE FROM transcriptions WHERE id = ?1", params![id])
            .map_err(|e| format!("Delete failed: {}", e))?;
        Ok(())
    }

    pub fn clear_history(&self) -> Result<usize, String> {
        let conn = self.connection()?;
        conn.execute("DELETE FROM transcriptions", [])
            .map_err(|e| format!("Clear failed: {}", e))
    }

    /// Drops anything older than `days`. Backs the optional retention setting.
    pub fn prune_older_than(&self, days: i64) -> Result<usize, String> {
        if days <= 0 {
            return Ok(0);
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM transcriptions WHERE created_at < ?1",
            params![cutoff],
        )
        .map_err(|e| format!("Prune failed: {}", e))
    }

    /// A key the user never set and a database that cannot answer are two
    /// different facts, and the settings that protect the user decide which way
    /// to fail from the difference. `Ok(None)` is "no such row"; `Err` is "we
    /// do not know what the row says" -- and a poisoned lock keeps saying so
    /// for the rest of the process's life, not just for the panic that caused
    /// it.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>, String> {
        self.connection()?
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("Setting read failed: {}", e))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.connection()?
            .execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(|e| format!("Setting save failed: {}", e))?;
        Ok(())
    }

    pub fn remove_setting(&self, key: &str) -> Result<(), String> {
        self.connection()?
            .execute("DELETE FROM settings WHERE key = ?1", params![key])
            .map_err(|e| format!("Setting delete failed: {}", e))?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn scratch_database() -> Database {
        let dir = std::env::temp_dir().join(format!("openflow-db-{}", uuid::Uuid::new_v4()));
        Database::new(dir).expect("a scratch database")
    }

    /// Leave the connection lock in the state a panic taken while holding it
    /// leaves it in. Nothing in the app poisons the lock deliberately, but any
    /// panic on a thread that holds it does, and from then on every read is a
    /// failure rather than an answer -- which is the case the settings above it
    /// have to survive.
    pub(crate) fn poison_the_connection_lock(db: &Database) {
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = db.conn.lock().expect("the lock is still good");
            panic!("poisoning the connection lock on purpose");
        }));
        assert!(panicked.is_err(), "the fixture has to panic to poison");
        assert!(
            db.connection().is_err(),
            "the lock is poisoned from here on"
        );
    }

    /// The distinction the privacy flags are built on: a key nobody wrote reads
    /// as absent, while a database that cannot be reached reads as a failure
    /// even for a key that is certainly there.
    #[test]
    fn an_absent_row_and_an_unreachable_database_read_differently() {
        let db = scratch_database();
        assert_eq!(db.get_setting("never_written"), Ok(None));
        db.set_setting("provider", "groq").expect("write a row");
        assert_eq!(db.get_setting("provider"), Ok(Some("groq".to_string())));

        poison_the_connection_lock(&db);

        assert!(
            db.get_setting("provider").is_err(),
            "a stored row that cannot be read must not come back as if unset"
        );
        assert!(db.get_setting("never_written").is_err());
    }

    use std::path::Path;

    fn scratch_dir() -> PathBuf {
        std::env::temp_dir().join(format!("openflow-db-{}", uuid::Uuid::new_v4()))
    }

    fn transcription(id: &str, marker: &str) -> Transcription {
        Transcription {
            id: id.to_string(),
            // Long enough that the row does not fit in the page header noise,
            // so "the bytes are in the file" is about the text and not about
            // an index entry that happens to repeat it.
            raw_text: format!("{marker}-said-{}", "a".repeat(200)),
            formatted_text: Some(format!("{marker}-cleaned-{}", "b".repeat(200))),
            provider: "test".to_string(),
            duration_ms: Some(1000),
            context_type: None,
            window_title: None,
            language: None,
            created_at: "2026-09-07T00:00:00Z".to_string(),
        }
    }

    /// How many times the raw bytes of the database contain `needle`.
    ///
    /// The whole file, not the rows: the question these tests ask is what is
    /// left on disk after the row is gone, and no query can answer that.
    fn times_in_the_file(path: &Path, needle: &str) -> usize {
        let bytes = std::fs::read(path).expect("read the database file");
        bytes
            .windows(needle.len())
            .filter(|window| *window == needle.as_bytes())
            .count()
    }

    /// Write a database the way a build without `secure_delete` wrote one.
    ///
    /// `Database::new` turns the pragma on, so the state this is reproducing
    /// cannot be reached through it any more -- and reproducing it is the only
    /// way to test the one-time scrub against the thing it exists for.
    fn database_with_deleted_text_still_in_it(dir: &Path, marker: &str) -> PathBuf {
        let path = {
            let db = Database::new(dir.to_path_buf()).expect("a database");
            let conn = db.connection().expect("the connection");
            conn.execute_batch("PRAGMA secure_delete = OFF")
                .expect("turn the pragma back off");
            for index in 0..40 {
                conn.execute(
                    "INSERT INTO transcriptions (id, raw_text, formatted_text, provider, created_at)
                     VALUES (?1, ?2, ?3, 'test', '2026-09-07T00:00:00Z')",
                    params![
                        format!("old{index}"),
                        format!("{marker}-said-{}", "a".repeat(200)),
                        format!("{marker}-cleaned-{}", "b".repeat(200)),
                    ],
                )
                .expect("insert");
            }
            conn.execute("DELETE FROM transcriptions", [])
                .expect("delete every row");
            conn.execute(
                "DELETE FROM settings WHERE key = ?1",
                params![Database::SCRUBBED],
            )
            .expect("un-mark the scrub");
            dir.join("openflow.db")
        };
        assert!(
            times_in_the_file(&path, marker) > 0,
            "the fixture did not manage to leave anything behind, so the test \
             below would pass on a database that was never dirty"
        );
        path
    }

    /// Deleting one dictation removes its words from the file.
    ///
    /// The comment above `delete_transcription` calls storing dictation with no
    /// way to remove it the largest privacy gap the app can have. Without the
    /// pragma the row disappears from every query and the text stays in the
    /// file -- measured: still there after the delete, and still there after
    /// the database is closed.
    #[test]
    fn deleting_a_dictation_takes_its_words_out_of_the_file() {
        let dir = scratch_dir();
        let db = Database::new(dir.clone()).expect("a database");
        let path = dir.join("openflow.db");

        db.save_transcription(&transcription("one", "KEPTSECRET"))
            .expect("save");
        assert!(
            times_in_the_file(&path, "KEPTSECRET") > 0,
            "the fixture never wrote the text, so the assertion below is empty"
        );

        db.delete_transcription("one").expect("delete");
        assert_eq!(
            times_in_the_file(&path, "KEPTSECRET"),
            0,
            "the dictation is gone from the history and still in the file"
        );

        drop(db);
        assert_eq!(
            times_in_the_file(&path, "KEPTSECRET"),
            0,
            "the text came back, or outlived the connection that deleted it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Clear history clears the file, not just the table.
    #[test]
    fn clearing_the_history_takes_every_dictation_out_of_the_file() {
        let dir = scratch_dir();
        let db = Database::new(dir.clone()).expect("a database");
        let path = dir.join("openflow.db");

        for index in 0..40 {
            db.save_transcription(&transcription(&format!("row{index}"), "CLEAREDSECRET"))
                .expect("save");
        }
        assert!(times_in_the_file(&path, "CLEAREDSECRET") > 0);

        let removed = db.clear_history().expect("clear");
        assert_eq!(removed, 40);
        assert_eq!(
            times_in_the_file(&path, "CLEAREDSECRET"),
            0,
            "Clear history emptied the list and left every dictation readable \
             in the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Retention does too. It deletes on a timer, without the user watching.
    #[test]
    fn pruning_takes_the_pruned_dictations_out_of_the_file() {
        let dir = scratch_dir();
        let db = Database::new(dir.clone()).expect("a database");
        let path = dir.join("openflow.db");

        for index in 0..40 {
            let mut row = transcription(&format!("old{index}"), "PRUNEDSECRET");
            row.created_at = "2020-01-01T00:00:00Z".to_string();
            db.save_transcription(&row).expect("save");
        }
        assert!(times_in_the_file(&path, "PRUNEDSECRET") > 0);

        let pruned = db.prune_older_than(30).expect("prune");
        assert_eq!(pruned, 40);
        assert_eq!(
            times_in_the_file(&path, "PRUNEDSECRET"),
            0,
            "retention dropped them from the list and left them in the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A database an earlier build left dirty is cleaned when it is opened.
    ///
    /// The pragma only governs deletes from the point it is set. Every user who
    /// has already deleted a dictation has that dictation in their file right
    /// now, and only rewriting the file removes it.
    #[test]
    fn opening_a_database_an_earlier_build_wrote_scrubs_it() {
        let dir = scratch_dir();
        let path = database_with_deleted_text_still_in_it(&dir, "LEGACYSECRET");

        let db = Database::new(dir.clone()).expect("reopen");
        assert_eq!(
            times_in_the_file(&path, "LEGACYSECRET"),
            0,
            "opening the database left the previous build's deleted text in it"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And does not rewrite the file again on every launch after that.
    ///
    /// `VACUUM` is an unbounded rewrite of the whole history, and once the
    /// pragma is on there is nothing left for a second one to find, so a scrub
    /// that ran every time would be pure cost -- on a large history, a visible
    /// one.
    ///
    /// The shape of this test is the whole test. Comparing the file size across
    /// two opens of an *already compact* database proves nothing: a `VACUUM`
    /// that runs on a file with no free pages leaves the size exactly as it
    /// was, so the assertion passes whether the scrub ran or not. It was
    /// written that way first, and the forgery -- scrub unconditionally --
    /// stayed green. So the database is left with free pages to reclaim before
    /// the second open, which is a state the two behaviours disagree about.
    #[test]
    fn the_scrub_runs_once_and_then_leaves_the_file_alone() {
        let dir = scratch_dir();
        let path = database_with_deleted_text_still_in_it(&dir, "ONCESECRET");

        let db = Database::new(dir.clone()).expect("first open");
        assert_eq!(times_in_the_file(&path, "ONCESECRET"), 0);
        for index in 0..60 {
            db.save_transcription(&transcription(&format!("new{index}"), "AFTERSECRET"))
                .expect("save");
        }
        let kept = db.get_history(500).expect("history").len();
        // Deleted with the pragma on, so the text is already gone and the only
        // thing a second VACUUM could still do is give the pages back.
        for index in 0..40 {
            db.delete_transcription(&format!("new{index}"))
                .expect("delete");
        }
        drop(db);
        let before = std::fs::metadata(&path).expect("metadata").len();

        let db = Database::new(dir.clone()).expect("second open");
        let after = std::fs::metadata(&path).expect("metadata").len();
        assert_eq!(
            before, after,
            "the second open rewrote the file, so every launch pays for a \
             VACUUM of the whole history"
        );
        assert_eq!(
            db.get_history(500).expect("history").len(),
            kept - 40,
            "the scrub marker is not worth losing history over"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
