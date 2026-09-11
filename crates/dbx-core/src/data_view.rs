use serde::{Deserialize, Serialize};

use std::collections::HashMap;

use crate::connection::AppState;
use crate::data_view_params::{substitute, DataViewParamValue, SubstituteDialect};
use crate::models::connection::DatabaseType;
use crate::query_cancel::RunningTaskMetadata;

fn default_display_mode() -> String {
    "table".to_string()
}

fn default_variable_kind() -> String {
    "string".to_string()
}

fn default_input_type() -> String {
    "text".to_string()
}

fn default_query_kind() -> String {
    "query".to_string()
}

/// A shared variable exposed to the data-view runner. Every sub-query in the
/// view resolves its `${name}` placeholders from this view-level set, so the
/// viewer only fills in one input per variable regardless of how many
/// sub-queries reference it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataViewVariable {
    pub name: String,
    #[serde(default)]
    pub label: String,
    /// One of `string` | `number` | `boolean` | `date`. Drives safe literal
    /// quoting during server-side substitution.
    #[serde(default = "default_variable_kind")]
    pub kind: String,
    /// `text` | `select`.
    #[serde(default = "default_input_type")]
    pub input_type: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default_value: Option<String>,
}

/// A saved query's position and size in the Runner's 12-column dashboard grid.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DataViewGridPos {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

/// One saved query inside a data view. Each sub-query carries its own
/// connection so a single view can span multiple data sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataViewQuery {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub catalog: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    /// SQL with `${name}` placeholders resolved from the view's variables.
    pub sql_template: String,
    /// `query` (read, default) or `mutation` (write: INSERT/UPDATE/DELETE).
    #[serde(default = "default_query_kind")]
    pub kind: String,
    /// Per-query override of the view's default display mode.
    #[serde(default)]
    pub display_mode: Option<String>,
    #[serde(default)]
    pub chart_config: Option<serde_json::Value>,
    #[serde(default)]
    pub order_index: i64,
    /// Runner dashboard-grid placement; absent until the user first edits the layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_pos: Option<DataViewGridPos>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataView {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_display_mode")]
    pub default_display_mode: String,
    #[serde(default)]
    pub queries: Vec<DataViewQuery>,
    #[serde(default)]
    pub variables: Vec<DataViewVariable>,
    /// Reserved for future ownership/permission enforcement; unused today.
    #[serde(default)]
    pub owner_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Lightweight row for the data-view list page. Omits `queries`/`variables`
/// so the listing stays cheap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataViewSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub default_display_mode: String,
    pub query_count: i64,
    #[serde(default)]
    pub owner_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Result of running one sub-query. Errors are isolated per query so one
/// failing sub-query never aborts the rest of the view.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataViewQueryResult {
    pub query_id: String,
    pub title: String,
    pub display_mode: String,
    pub result: Option<crate::db::QueryResult>,
    /// Raw Redis command result (see `redis_ops::redis_execute_command_core`), returned instead of
    /// `result` for Redis queries — the frontend already owns the value→table conversion heuristics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_value: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteDataViewResponse {
    pub results: Vec<DataViewQueryResult>,
}

#[derive(Debug, Clone, Default)]
pub struct DataViewExecuteOptions {
    pub max_rows: Option<usize>,
    pub timeout_secs: Option<u64>,
    pub client_session_id: Option<String>,
    /// When set, only sub-queries with these ids run. Used to run reads and
    /// writes independently (mutations execute one at a time on demand).
    pub query_ids: Option<Vec<String>>,
}

/// Runs every sub-query in `view` after substituting `values` into each
/// `${name}` placeholder. Shared by the web and desktop backends.
pub async fn execute_data_view(
    state: &AppState,
    view: &DataView,
    values: &HashMap<String, DataViewParamValue>,
    opts: &DataViewExecuteOptions,
) -> ExecuteDataViewResponse {
    let mut queries = view.queries.clone();
    queries.sort_by_key(|q| q.order_index);
    if let Some(ids) = &opts.query_ids {
        queries.retain(|q| ids.contains(&q.id));
    }

    let mut results = Vec::with_capacity(queries.len());
    for query in queries {
        let display_mode = query.display_mode.clone().unwrap_or_else(|| view.default_display_mode.clone());
        let config = state.configs.read().await.get(&query.connection_id).cloned();
        let is_redis = matches!(config.map(|c| c.db_type), Some(DatabaseType::Redis));
        let dialect = if is_redis { SubstituteDialect::Redis } else { SubstituteDialect::Sql };
        let sql = match substitute(&query.sql_template, &view.variables, values, dialect) {
            Ok(sql) => sql,
            Err(err) => {
                results.push(DataViewQueryResult {
                    query_id: query.id.clone(),
                    title: query.title.clone(),
                    display_mode,
                    result: None,
                    redis_value: None,
                    error: Some(err.to_string()),
                });
                continue;
            }
        };

        if is_redis {
            let db = query.database.trim().parse::<u32>().unwrap_or(0);
            // Redis commands are near-instant and `redis_execute_command_core` has no
            // cancellation hook, so this branch skips the running-task registration below.
            match crate::redis_ops::redis_execute_command_core(state, &query.connection_id, db, &sql, false).await {
                Ok(result) => results.push(DataViewQueryResult {
                    query_id: query.id.clone(),
                    title: query.title.clone(),
                    display_mode,
                    result: None,
                    redis_value: Some(result.value),
                    error: None,
                }),
                Err(error) => results.push(DataViewQueryResult {
                    query_id: query.id.clone(),
                    title: query.title.clone(),
                    display_mode,
                    result: None,
                    redis_value: None,
                    error: Some(error),
                }),
            }
            continue;
        }

        let execution_id = uuid::Uuid::new_v4().to_string();
        let registered = state.running_queries.register_task(
            execution_id.clone(),
            RunningTaskMetadata::query(
                query.connection_id.clone(),
                query.database.clone(),
                opts.client_session_id.clone(),
            ),
        );
        let cancel_token = registered.token();

        let outcome = crate::query::execute_sql_statement_with_options_typed(
            state,
            &query.connection_id,
            &query.database,
            &sql,
            query.schema.as_deref(),
            Some(cancel_token),
            crate::query::QueryExecutionOptions {
                max_rows: opts.max_rows,
                catalog: query.catalog.clone(),
                client_session_id: opts.client_session_id.clone(),
                timeout_secs: opts.timeout_secs,
                execution_id: Some(execution_id),
                ..Default::default()
            },
        )
        .await;

        registered.finish(&outcome);

        match outcome {
            Ok(result) => results.push(DataViewQueryResult {
                query_id: query.id.clone(),
                title: query.title.clone(),
                display_mode,
                result: Some(result),
                redis_value: None,
                error: None,
            }),
            Err(error) => results.push(DataViewQueryResult {
                query_id: query.id.clone(),
                title: query.title.clone(),
                display_mode,
                result: None,
                redis_value: None,
                error: Some(error.into_legacy_string()),
            }),
        }
    }

    ExecuteDataViewResponse { results }
}
