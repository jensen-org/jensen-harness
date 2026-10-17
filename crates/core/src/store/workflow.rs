//! Workflow runs, content addressed artifacts and memoized node results.
//! Like the journal, none of it is ever updated or deleted.

use rusqlite::{OptionalExtension, params};

use super::{Store, StoreError, now_ms};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    pub run_id: String,
    pub spec_hash: String,
    pub spec: String,
    pub inputs: String,
}

impl Store {
    /// Stores `bytes` under their blake3 hash and returns the hash. Storing
    /// the same bytes twice is a no-op.
    ///
    /// # Errors
    /// [`StoreError`] when the write fails.
    pub async fn put_artifact(&self, bytes: Vec<u8>) -> Result<String, StoreError> {
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let key = hash.clone();
        self.with(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO artifacts (hash, bytes, created_at_ms) VALUES (?1, ?2, ?3)",
                params![key, bytes, now_ms()?],
            )?;
            Ok(())
        })
        .await?;
        Ok(hash)
    }

    /// # Errors
    /// [`StoreError`] when the read fails or the stored bytes no longer
    /// match their hash.
    pub async fn artifact(&self, hash: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let key = hash.to_owned();
        let found: Option<Vec<u8>> = self
            .with(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT bytes FROM artifacts WHERE hash = ?1",
                        params![key],
                        |row| row.get(0),
                    )
                    .optional()?)
            })
            .await?;
        if let Some(bytes) = &found
            && blake3::hash(bytes).to_hex().as_str() != hash
        {
            return Err(StoreError::Corrupt(format!(
                "artifact {hash} does not match its hash"
            )));
        }
        Ok(found)
    }

    /// # Errors
    /// [`StoreError`] when the run already exists or the write fails.
    pub async fn create_workflow_run(&self, run: WorkflowRun) -> Result<(), StoreError> {
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO workflow_runs (run_id, spec_hash, spec, inputs, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run.run_id, run.spec_hash, run.spec, run.inputs, now_ms()?],
            )?;
            Ok(())
        })
        .await
    }

    /// # Errors
    /// [`StoreError`] when the read fails.
    pub async fn workflow_run(&self, run_id: &str) -> Result<Option<WorkflowRun>, StoreError> {
        let key = run_id.to_owned();
        self.with(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT run_id, spec_hash, spec, inputs FROM workflow_runs WHERE run_id = ?1",
                    params![key],
                    |row| {
                        Ok(WorkflowRun {
                            run_id: row.get(0)?,
                            spec_hash: row.get(1)?,
                            spec: row.get(2)?,
                            inputs: row.get(3)?,
                        })
                    },
                )
                .optional()?)
        })
        .await
    }

    /// # Errors
    /// [`StoreError`] when the run is unknown or the write fails.
    pub async fn record_node_result(
        &self,
        run_id: &str,
        node_id: &str,
        memo_key: &str,
        outputs: &str,
    ) -> Result<(), StoreError> {
        let (run_id, node_id, memo_key, outputs) = (
            run_id.to_owned(),
            node_id.to_owned(),
            memo_key.to_owned(),
            outputs.to_owned(),
        );
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO node_results (run_id, node_id, memo_key, outputs, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run_id, node_id, memo_key, outputs, now_ms()?],
            )?;
            Ok(())
        })
        .await
    }

    /// The outputs most recently recorded for `memo_key`, if any.
    ///
    /// # Errors
    /// [`StoreError`] when the read fails.
    pub async fn memoized_result(&self, memo_key: &str) -> Result<Option<String>, StoreError> {
        let key = memo_key.to_owned();
        self.with(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT outputs FROM node_results WHERE memo_key = ?1 ORDER BY seq DESC LIMIT 1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await
    }
}
