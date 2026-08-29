use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use sqlx::{PgPool, Postgres, QueryBuilder, Row, postgres::PgPoolOptions};
use tokio::sync::Notify;

use super::trace_db::{FailureStats, TraceAttempt, TraceKeySource, TraceQuery, TraceRecord};

struct PendingWriteGuard {
    ticket: u64,
    pending: Arc<parking_lot::Mutex<BTreeSet<u64>>>,
    notify: Arc<Notify>,
}

impl Drop for PendingWriteGuard {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.ticket);
        self.notify.notify_waiters();
    }
}

#[derive(Clone)]
pub struct PostgresTraceStore {
    pool: PgPool,
    next_write_ticket: Arc<AtomicU64>,
    pending_writes: Arc<parking_lot::Mutex<BTreeSet<u64>>>,
    write_notify: Arc<Notify>,
}

impl PostgresTraceStore {
    pub async fn connect(database_url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await
            .context("连接 PostgreSQL Trace 数据库失败")?;
        let store = Self {
            pool,
            next_write_ticket: Arc::new(AtomicU64::new(0)),
            pending_writes: Arc::new(parking_lot::Mutex::new(BTreeSet::new())),
            write_notify: Arc::new(Notify::new()),
        };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        for statement in POSTGRES_SCHEMA {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub fn spawn_insert(&self, record: TraceRecord) {
        let ticket = {
            let mut pending = self.pending_writes.lock();
            let ticket = self.next_write_ticket.fetch_add(1, Ordering::AcqRel) + 1;
            pending.insert(ticket);
            ticket
        };
        let store = self.clone();
        let guard = PendingWriteGuard {
            ticket,
            pending: store.pending_writes.clone(),
            notify: store.write_notify.clone(),
        };
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(error) = store.insert(&record).await {
                tracing::warn!("PostgreSQL trace 写入失败: {}", error);
            }
        });
    }

    /// 等待调用本方法前已排队的写入；之后到达的新写入不会延长本次等待。
    async fn wait_for_pending_writes(&self) {
        let target = self.next_write_ticket.load(Ordering::Acquire);
        loop {
            let notified = self.write_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let has_older_pending = self.pending_writes.lock().range(..=target).next().is_some();
            if !has_older_pending {
                return;
            }
            notified.await;
        }
    }

    pub async fn insert(&self, rec: &TraceRecord) -> anyhow::Result<()> {
        let ts_epoch = chrono::DateTime::parse_from_rfc3339(&rec.ts)
            .map(|value| value.timestamp())
            .unwrap_or_else(|_| chrono::Utc::now().timestamp());
        let mut tx = self.pool.begin().await?;
        sqlx::query(UPSERT_TRACE)
            .bind(&rec.trace_id)
            .bind(&rec.ts)
            .bind(ts_epoch)
            .bind(db_i64(rec.key_id))
            .bind(rec.key_source.as_str())
            .bind(&rec.model)
            .bind(rec.is_stream)
            .bind(&rec.final_status)
            .bind(db_i64(rec.final_credential_id))
            .bind(&rec.error_type)
            .bind(&rec.error_message)
            .bind(i64::from(rec.total_attempts))
            .bind(db_i64(rec.duration_ms))
            .bind(rec.interrupted_after_bytes.map(db_i64))
            .bind(db_i64(rec.input_tokens))
            .bind(db_i64(rec.output_tokens))
            .bind(db_i64(rec.cache_creation_tokens))
            .bind(db_i64(rec.cache_read_tokens))
            .bind(rec.credits)
            .bind(rec.first_token_ms.map(db_i64))
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM trace_attempts WHERE trace_id = $1")
            .bind(&rec.trace_id)
            .execute(&mut *tx)
            .await?;
        for attempt in &rec.attempts {
            sqlx::query(INSERT_ATTEMPT)
                .bind(&rec.trace_id)
                .bind(i64::from(attempt.attempt))
                .bind(db_i64(attempt.credential_id))
                .bind(&attempt.endpoint)
                .bind(attempt.http_status.map(i32::from))
                .bind(&attempt.outcome)
                .bind(&attempt.error_snippet)
                .bind(db_i64(attempt.duration_ms))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn query_paged(
        &self,
        query: &TraceQuery,
    ) -> anyhow::Result<(Vec<TraceRecord>, usize)> {
        self.wait_for_pending_writes().await;
        let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM traces");
        append_where(&mut count, query);
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;

        let mut select = QueryBuilder::<Postgres>::new(
            "SELECT trace_id, ts, key_id, key_source, model, is_stream, final_status, \
             final_credential_id, error_type, error_message, total_attempts, duration_ms, \
             interrupted_after_bytes, input_tokens, output_tokens, cache_creation_tokens, \
             cache_read_tokens, credits, first_token_ms FROM traces",
        );
        append_where(&mut select, query);
        select
            .push(" ORDER BY ts_epoch DESC LIMIT ")
            .push_bind(if query.limit == 0 {
                200_i64
            } else {
                query.limit as i64
            })
            .push(" OFFSET ")
            .push_bind(query.offset as i64);
        let rows = select.build().fetch_all(&self.pool).await?;
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let source: String = row.try_get(3)?;
            let mut record = TraceRecord {
                trace_id: row.try_get(0)?,
                ts: row.try_get(1)?,
                key_id: app_u64(row.try_get(2)?),
                key_source: parse_key_source(&source)?,
                model: row.try_get(4)?,
                is_stream: row.try_get(5)?,
                final_status: row.try_get(6)?,
                final_credential_id: app_u64(row.try_get(7)?),
                error_type: row.try_get(8)?,
                error_message: row.try_get(9)?,
                total_attempts: app_u64(row.try_get(10)?).min(u32::MAX as u64) as u32,
                duration_ms: app_u64(row.try_get(11)?),
                interrupted_after_bytes: row.try_get::<Option<i64>, _>(12)?.map(app_u64),
                input_tokens: app_u64(row.try_get(13)?),
                output_tokens: app_u64(row.try_get(14)?),
                cache_creation_tokens: app_u64(row.try_get(15)?),
                cache_read_tokens: app_u64(row.try_get(16)?),
                credits: row.try_get(17)?,
                first_token_ms: row.try_get::<Option<i64>, _>(18)?.map(app_u64),
                attempts: Vec::new(),
            };
            record.attempts = self.attempts_for(&record.trace_id).await?;
            records.push(record);
        }
        Ok((records, total.max(0) as usize))
    }

    async fn attempts_for(&self, trace_id: &str) -> anyhow::Result<Vec<TraceAttempt>> {
        let rows = sqlx::query(
            "SELECT attempt, credential_id, endpoint, http_status, outcome, error_snippet, \
             duration_ms FROM trace_attempts WHERE trace_id = $1 ORDER BY attempt ASC",
        )
        .bind(trace_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TraceAttempt {
                    attempt: app_u64(row.try_get(0)?).min(u32::MAX as u64) as u32,
                    credential_id: app_u64(row.try_get(1)?),
                    endpoint: row.try_get(2)?,
                    http_status: row.try_get::<Option<i32>, _>(3)?.map(|v| v.max(0) as u16),
                    outcome: row.try_get(4)?,
                    error_snippet: row.try_get(5)?,
                    duration_ms: app_u64(row.try_get(6)?),
                })
            })
            .collect()
    }

    pub async fn cleanup(&self, cutoff: i64) -> anyhow::Result<u64> {
        self.wait_for_pending_writes().await;
        let result = sqlx::query("DELETE FROM traces WHERE ts_epoch < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_for_credential(&self, credential_id: u64) -> anyhow::Result<u64> {
        self.wait_for_pending_writes().await;
        let id = db_i64(credential_id);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM trace_attempts WHERE credential_id = $1 OR trace_id IN \
             (SELECT trace_id FROM traces WHERE final_credential_id = $1)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        let result = sqlx::query("DELETE FROM traces WHERE final_credential_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    pub async fn failure_stats(&self) -> anyhow::Result<HashMap<u64, FailureStats>> {
        self.wait_for_pending_writes().await;
        let rows = sqlx::query(
            "SELECT credential_id, outcome, COUNT(*) AS count FROM trace_attempts \
             WHERE outcome <> 'success' AND credential_id <> 0 GROUP BY credential_id, outcome",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut stats = HashMap::new();
        for row in rows {
            let id = app_u64(row.try_get(0)?);
            let outcome: String = row.try_get(1)?;
            let count = app_u64(row.try_get(2)?);
            let item: &mut FailureStats = stats.entry(id).or_default();
            match outcome.as_str() {
                "auth_failed" => item.auth += count,
                "account_throttled" => item.throttle += count,
                _ => item.other += count,
            }
        }
        Ok(stats)
    }
}

const POSTGRES_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS traces (
        trace_id TEXT PRIMARY KEY, ts TEXT NOT NULL, ts_epoch BIGINT NOT NULL,
        key_id BIGINT NOT NULL, key_source TEXT NOT NULL, model TEXT NOT NULL,
        is_stream BOOLEAN NOT NULL, final_status TEXT NOT NULL,
        final_credential_id BIGINT NOT NULL, error_type TEXT, error_message TEXT,
        total_attempts BIGINT NOT NULL, duration_ms BIGINT NOT NULL,
        interrupted_after_bytes BIGINT, input_tokens BIGINT NOT NULL DEFAULT 0,
        output_tokens BIGINT NOT NULL DEFAULT 0,
        cache_creation_tokens BIGINT NOT NULL DEFAULT 0,
        cache_read_tokens BIGINT NOT NULL DEFAULT 0,
        credits DOUBLE PRECISION NOT NULL DEFAULT 0, first_token_ms BIGINT
    )",
    "CREATE INDEX IF NOT EXISTS idx_traces_ts ON traces(ts_epoch DESC)",
    "CREATE INDEX IF NOT EXISTS idx_traces_status ON traces(final_status)",
    "CREATE INDEX IF NOT EXISTS idx_traces_cred ON traces(final_credential_id)",
    "CREATE TABLE IF NOT EXISTS trace_attempts (
        trace_id TEXT NOT NULL REFERENCES traces(trace_id) ON DELETE CASCADE,
        attempt BIGINT NOT NULL, credential_id BIGINT NOT NULL,
        endpoint TEXT NOT NULL, http_status INTEGER, outcome TEXT NOT NULL,
        error_snippet TEXT, duration_ms BIGINT NOT NULL,
        PRIMARY KEY (trace_id, attempt)
    )",
    "CREATE INDEX IF NOT EXISTS idx_attempts_trace ON trace_attempts(trace_id)",
    "CREATE INDEX IF NOT EXISTS idx_attempts_credential ON trace_attempts(credential_id)",
];

const UPSERT_TRACE: &str = "INSERT INTO traces (
    trace_id, ts, ts_epoch, key_id, key_source, model, is_stream, final_status,
    final_credential_id, error_type, error_message, total_attempts, duration_ms,
    interrupted_after_bytes, input_tokens, output_tokens, cache_creation_tokens,
    cache_read_tokens, credits, first_token_ms
) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)
ON CONFLICT (trace_id) DO UPDATE SET
    ts=EXCLUDED.ts, ts_epoch=EXCLUDED.ts_epoch, key_id=EXCLUDED.key_id,
    key_source=EXCLUDED.key_source, model=EXCLUDED.model, is_stream=EXCLUDED.is_stream,
    final_status=EXCLUDED.final_status, final_credential_id=EXCLUDED.final_credential_id,
    error_type=EXCLUDED.error_type, error_message=EXCLUDED.error_message,
    total_attempts=EXCLUDED.total_attempts, duration_ms=EXCLUDED.duration_ms,
    interrupted_after_bytes=EXCLUDED.interrupted_after_bytes,
    input_tokens=EXCLUDED.input_tokens, output_tokens=EXCLUDED.output_tokens,
    cache_creation_tokens=EXCLUDED.cache_creation_tokens,
    cache_read_tokens=EXCLUDED.cache_read_tokens, credits=EXCLUDED.credits,
    first_token_ms=EXCLUDED.first_token_ms";

const INSERT_ATTEMPT: &str = "INSERT INTO trace_attempts (
    trace_id, attempt, credential_id, endpoint, http_status, outcome, error_snippet, duration_ms
) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)";

fn db_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn app_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn parse_key_source(value: &str) -> anyhow::Result<TraceKeySource> {
    match value {
        "masterApiKey" => Ok(TraceKeySource::MasterApiKey),
        "clientKey" => Ok(TraceKeySource::ClientKey),
        other => anyhow::bail!("未知 trace key_source: {other}"),
    }
}

fn append_credential_ids<'a>(builder: &mut QueryBuilder<'a, Postgres>, ids: Option<&'a [u64]>) {
    let Some(ids) = ids else { return };
    if ids.is_empty() {
        builder.push(" AND FALSE");
        return;
    }
    builder.push(" AND final_credential_id IN (");
    let mut separated = builder.separated(", ");
    for id in ids {
        separated.push_bind(db_i64(*id));
    }
    separated.push_unseparated(")");
}

fn append_keyword<'a>(builder: &mut QueryBuilder<'a, Postgres>, keyword: Option<&'a str>) {
    let Some(keyword) = keyword else { return };
    let pattern = format!(
        "%{}%",
        keyword
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    builder
        .push(" AND (model ILIKE ")
        .push_bind(pattern.clone())
        .push(" ESCAPE '\\' OR trace_id ILIKE ")
        .push_bind(pattern.clone())
        .push(" ESCAPE '\\' OR COALESCE(error_message, '') ILIKE ")
        .push_bind(pattern)
        .push(" ESCAPE '\\')");
}

fn append_where<'a>(builder: &mut QueryBuilder<'a, Postgres>, query: &'a TraceQuery) {
    builder.push(" WHERE TRUE");
    if let Some(value) = &query.status {
        builder.push(" AND final_status = ").push_bind(value);
    }
    if let Some(value) = &query.error_type {
        builder.push(" AND error_type = ").push_bind(value);
    }
    if let Some(value) = query.credential_id {
        builder
            .push(" AND final_credential_id = ")
            .push_bind(db_i64(value));
    }
    if let Some(value) = query.key_id {
        builder.push(" AND key_id = ").push_bind(db_i64(value));
    }
    if let Some(value) = query.failed_attempt_credential_id {
        builder.push(" AND EXISTS (SELECT 1 FROM trace_attempts a WHERE a.trace_id = traces.trace_id AND a.credential_id = ")
            .push_bind(db_i64(value)).push(" AND a.outcome <> 'success')");
    }
    if let Some(value) = &query.model {
        builder.push(" AND model = ").push_bind(value);
    }
    append_credential_ids(builder, query.credential_ids.as_deref());
    if query.only_failed {
        builder.push(" AND final_status <> 'success'");
    }
    if let Some(value) = query.start_ts {
        builder.push(" AND ts_epoch >= ").push_bind(value);
    }
    if let Some(value) = query.end_ts {
        builder.push(" AND ts_epoch <= ").push_bind(value);
    }
    append_keyword(builder, query.keyword.as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use sqlx::Execute;
    use uuid::Uuid;

    fn sample(trace_id: String, credential_id: u64, ts: String) -> TraceRecord {
        TraceRecord {
            trace_id,
            ts,
            key_id: 7,
            key_source: TraceKeySource::ClientKey,
            model: "postgres-test-model".into(),
            is_stream: true,
            final_status: "error".into(),
            final_credential_id: credential_id,
            error_type: Some("auth_failed".into()),
            error_message: Some("postgres smoke marker".into()),
            total_attempts: 1,
            duration_ms: 42,
            interrupted_after_bytes: None,
            input_tokens: 11,
            output_tokens: 3,
            cache_creation_tokens: 5,
            cache_read_tokens: 7,
            credits: 0.25,
            first_token_ms: Some(9),
            attempts: vec![TraceAttempt {
                attempt: 0,
                credential_id,
                endpoint: "ide".into(),
                http_status: Some(401),
                outcome: "auth_failed".into(),
                error_snippet: Some("denied".into()),
                duration_ms: 40,
            }],
        }
    }

    #[test]
    fn postgres_where_uses_bound_parameters_for_all_filters() {
        let query = TraceQuery {
            status: Some("error".into()),
            error_type: Some("auth_failed".into()),
            credential_id: Some(9),
            key_id: Some(7),
            failed_attempt_credential_id: Some(8),
            model: Some("model-x".into()),
            only_failed: true,
            credential_ids: Some(vec![9, 10]),
            start_ts: Some(100),
            end_ts: Some(200),
            keyword: Some("x%_\\y".into()),
            limit: 20,
            offset: 3,
        };
        let mut builder = QueryBuilder::<Postgres>::new("SELECT * FROM traces");
        append_where(&mut builder, &query);
        let sql = builder.build().sql().to_string();
        assert!(sql.contains("final_status = $1"));
        assert!(sql.contains("EXISTS (SELECT 1 FROM trace_attempts"));
        assert!(sql.contains("final_credential_id IN"));
        assert!(sql.contains("ILIKE"));
        assert!(!sql.contains("model-x"), "filter values must remain bound");
        assert!(!sql.contains("x%_\\y"), "keyword must remain bound");
    }

    #[tokio::test]
    async fn postgres_roundtrip_when_test_url_is_set() {
        let Ok(url) = std::env::var("KIRO_TEST_POSTGRES_URL") else {
            return;
        };
        let store = PostgresTraceStore::connect(&url).await.unwrap();
        let suffix = Uuid::new_v4().simple().to_string();
        let credential_id = 8_000_000_000_u64
            + u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) % 1_000_000;
        let current_id = format!("pg-current-{suffix}");
        let old_id = format!("pg-old-{suffix}");

        store.spawn_insert(sample(
            current_id.clone(),
            credential_id,
            Utc::now().to_rfc3339(),
        ));
        for index in 0..15 {
            store.spawn_insert(sample(
                format!("pg-concurrent-{index}-{suffix}"),
                credential_id,
                Utc::now().to_rfc3339(),
            ));
        }
        store
            .insert(&sample(
                old_id.clone(),
                credential_id + 1,
                (Utc::now() - Duration::days(10)).to_rfc3339(),
            ))
            .await
            .unwrap();

        let (records, total) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            store.query_paged(&TraceQuery {
                credential_id: Some(credential_id),
                keyword: Some("smoke marker".into()),
                limit: 20,
                ..Default::default()
            }),
        )
        .await
        .expect("等待后台 PostgreSQL Trace 写入不应挂起")
        .unwrap();
        assert_eq!(total, 16);
        assert!(records.iter().any(|record| record.trace_id == current_id));
        assert!(records.iter().all(|record| record.attempts.len() == 1));
        assert!(records.iter().all(|record| record.cache_read_tokens == 7));
        assert_eq!(
            store.failure_stats().await.unwrap()[&credential_id].auth,
            16
        );

        let cutoff = (Utc::now() - Duration::days(7)).timestamp();
        assert_eq!(store.cleanup(cutoff).await.unwrap(), 1);
        assert_eq!(
            store.delete_for_credential(credential_id).await.unwrap(),
            16
        );
        let (remaining, _) = store
            .query_paged(&TraceQuery {
                keyword: Some(suffix),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(remaining.is_empty());
    }
}
