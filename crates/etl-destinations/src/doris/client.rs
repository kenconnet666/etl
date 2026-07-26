//! Doris Stream Load HTTP client.

use std::time::Duration;

use etl::{
    error::{ErrorKind, EtlResult},
    etl_error,
};
use reqwest::{Client, Url, header, redirect::Policy};
use serde::Deserialize;
use tracing::debug;

use crate::doris::{DorisTableName, config::DorisConfig};

/// Maximum number of frontend redirects followed manually.
///
/// The frontend answers a Stream Load with a single redirect to the coordinator
/// backend, so one hop is the normal case and the rest is headroom.
const MAX_REDIRECTS: usize = 3;

/// Update mode that limits a load to the columns it declares.
///
/// This is how Doris 4.x expresses a partial column update on a merge-on-write
/// unique-key table.
const PARTIAL_UPDATE_MODE: &str = "UPDATE_FIXED_COLUMNS";

/// Stream Load response from Doris.
///
/// See <https://doris.apache.org/docs/data-operate/import/import-way/stream-load-manual>.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StreamLoadResponse {
    status: String,
    message: Option<String>,
    number_total_rows: Option<i64>,
    number_loaded_rows: Option<i64>,
    number_filtered_rows: Option<i64>,
    error_url: Option<String>,
    /// State of the job that already owns the label, present only when
    /// `status` is `Label Already Exists`.
    existing_job_status: Option<String>,
}

/// Outcome of one Stream Load attempt.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StreamLoadOutcome {
    /// The batch is committed. Its data may still be becoming visible.
    Committed,
    /// An earlier attempt of the same label is still running, so the caller
    /// should wait and retry rather than sending a new label.
    InProgress,
}

/// HTTP client for the Doris Stream Load endpoint.
#[derive(Clone)]
pub(super) struct DorisStreamLoadClient {
    client: Client,
    config: DorisConfig,
    authorization: String,
}

impl DorisStreamLoadClient {
    /// Creates a client that follows frontend redirects itself.
    ///
    /// Redirects are handled manually because the frontend sends the request on
    /// to a backend on a different host, and an HTTP client that follows the
    /// redirect automatically drops the `Authorization` header on a cross-host
    /// hop. This is the same reason `curl` needs `--location-trusted`.
    pub fn new(config: DorisConfig) -> EtlResult<Self> {
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(config.stream_load_timeout_secs))
            .build()
            .map_err(|source| {
                etl_error!(
                    ErrorKind::DestinationError,
                    "Doris HTTP client creation failed",
                    source: source
                )
            })?;
        let authorization = basic_authorization(&config.user, &config.password);

        Ok(Self { client, config, authorization })
    }

    /// Loads one JSON batch and returns whether it is committed.
    pub async fn stream_load(
        &self,
        table_name: &DorisTableName,
        label: &str,
        columns_header: &str,
        partial_columns: bool,
        body: Vec<u8>,
    ) -> EtlResult<StreamLoadOutcome> {
        let mut url = self.config.stream_load_url(table_name.table())?;

        for _ in 0..=MAX_REDIRECTS {
            let response = self
                .send(&url, table_name, label, columns_header, partial_columns, body.clone())
                .await?;

            if let Some(location) = redirect_location(&response, &url, table_name, label)? {
                debug!(table = %table_name, label, "following doris stream load redirect");
                url = location;
                continue;
            }

            return self.read_outcome(response, table_name, label).await;
        }

        Err(etl_error!(
            ErrorKind::DestinationError,
            "Doris stream load exceeded the redirect limit",
            format!("table={table_name} label={label} max_redirects={MAX_REDIRECTS}")
        ))
    }

    /// Sends one Stream Load request to a frontend or backend.
    async fn send(
        &self,
        url: &Url,
        table_name: &DorisTableName,
        label: &str,
        columns_header: &str,
        partial_columns: bool,
        body: Vec<u8>,
    ) -> EtlResult<reqwest::Response> {
        let mut request = self
            .client
            .put(url.clone())
            .header(header::AUTHORIZATION, &self.authorization)
            .header(header::EXPECT, "100-continue")
            .header("format", "json")
            .header("strip_outer_array", "true")
            .header("label", label)
            .header("columns", columns_header)
            .header("timeout", self.config.stream_load_timeout_secs.to_string());

        if partial_columns {
            request = request
                .header("unique_key_update_mode", PARTIAL_UPDATE_MODE)
                .header("partial_update_new_key_behavior", "APPEND");
        }

        request.body(body).send().await.map_err(|source| {
            etl_error!(
                ErrorKind::DestinationError,
                "Doris stream load request failed",
                format!("table={table_name} label={label}"),
                source: source
            )
        })
    }

    /// Turns a terminal Stream Load response into an outcome.
    async fn read_outcome(
        &self,
        response: reqwest::Response,
        table_name: &DorisTableName,
        label: &str,
    ) -> EtlResult<StreamLoadOutcome> {
        let status_code = response.status();
        let body = response.text().await.map_err(|source| {
            etl_error!(
                ErrorKind::DestinationError,
                "Doris stream load response read failed",
                format!("table={table_name} label={label} http_status={status_code}"),
                source: source
            )
        })?;

        if !status_code.is_success() {
            return Err(etl_error!(
                ErrorKind::DestinationError,
                "Doris stream load returned an HTTP error",
                format!("table={table_name} label={label} http_status={status_code}")
            ));
        }

        classify_response(&body, table_name, label)
    }
}

/// Returns the redirect target of a Stream Load response, if there is one.
fn redirect_location(
    response: &reqwest::Response,
    current_url: &Url,
    table_name: &DorisTableName,
    label: &str,
) -> EtlResult<Option<Url>> {
    if !response.status().is_redirection() {
        return Ok(None);
    }

    let location = response.headers().get(header::LOCATION).ok_or_else(|| {
        etl_error!(
            ErrorKind::DestinationError,
            "Doris stream load redirect has no location",
            format!("table={table_name} label={label} http_status={}", response.status())
        )
    })?;
    let location = location.to_str().map_err(|source| {
        etl_error!(
            ErrorKind::DestinationError,
            "Doris stream load redirect location is not valid text",
            format!("table={table_name} label={label}"),
            source: source
        )
    })?;
    let location = current_url.join(location).map_err(|source| {
        etl_error!(
            ErrorKind::DestinationError,
            "Doris stream load redirect location is not a valid URL",
            format!("table={table_name} label={label}"),
            source: source
        )
    })?;

    Ok(Some(location))
}

/// Classifies a Stream Load response body.
///
/// `Success` and `Publish Timeout` both mean the transaction committed;
/// `Publish Timeout` only says visibility is still propagating. A duplicate
/// label proves an earlier attempt of the same batch already reached Doris,
/// which is what makes ETL's at-least-once retries safe, but the caller still
/// has to wait when that earlier attempt is running.
fn classify_response(
    body: &str,
    table_name: &DorisTableName,
    label: &str,
) -> EtlResult<StreamLoadOutcome> {
    let response: StreamLoadResponse = serde_json::from_str(body).map_err(|source| {
        etl_error!(
            ErrorKind::DestinationError,
            "Doris stream load response is not valid JSON",
            format!("table={table_name} label={label}"),
            source: source
        )
    })?;

    match response.status.as_str() {
        "Success" | "Publish Timeout" => {
            debug!(
                table = %table_name,
                label,
                status = %response.status,
                total_rows = response.number_total_rows.unwrap_or_default(),
                loaded_rows = response.number_loaded_rows.unwrap_or_default(),
                "doris stream load committed"
            );

            Ok(StreamLoadOutcome::Committed)
        }
        "Label Already Exists" => match response.existing_job_status.as_deref() {
            Some("FINISHED") => {
                debug!(
                    table = %table_name,
                    label,
                    "doris stream load already committed under this label"
                );

                Ok(StreamLoadOutcome::Committed)
            }
            _ => Ok(StreamLoadOutcome::InProgress),
        },
        _ => Err(etl_error!(
            ErrorKind::DestinationError,
            "Doris stream load did not succeed",
            format!(
                "table={table_name} label={label} status={} message={} error_url={} \
                 filtered_rows={}",
                response.status,
                response.message.as_deref().unwrap_or("<none>"),
                response.error_url.as_deref().unwrap_or("<none>"),
                response.number_filtered_rows.unwrap_or_default()
            )
        )),
    }
}

/// Builds an HTTP Basic authorization header value.
fn basic_authorization(user: &str, password: &str) -> String {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));

    format!("Basic {encoded}")
}

/// Builds the deterministic Stream Load label for a streaming batch.
///
/// Doris keeps labels for `label_keep_max_second` and refuses a repeat, so a
/// label derived from the source position turns an ETL retry into a no-op.
pub(super) fn build_stream_load_label(
    pipeline_id: u64,
    table: &str,
    commit_lsn: u64,
    tx_ordinal: u64,
    batch_index: u32,
) -> String {
    format!("etl_{pipeline_id}_{table}_{commit_lsn:016x}_{tx_ordinal:016x}_{batch_index}")
}

/// Builds the Stream Load label for a table-copy batch.
///
/// The run nonce keeps the label distinct from an earlier run's, because a copy
/// recreates the table and must not be skipped as a duplicate.
pub(super) fn build_copy_stream_load_label(
    pipeline_id: u64,
    table: &str,
    run_nonce: u64,
    batch_sequence: u64,
) -> String {
    format!("etl_copy_{pipeline_id}_{table}_{run_nonce:016x}_{batch_sequence:016x}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn table_name() -> DorisTableName {
        DorisTableName::new("analytics", "public_users")
    }

    #[test]
    fn committed_statuses_are_accepted() {
        for status in ["Success", "Publish Timeout"] {
            let body = json!({ "Status": status }).to_string();
            let outcome = classify_response(&body, &table_name(), "label-1").unwrap();

            assert_eq!(outcome, StreamLoadOutcome::Committed);
        }
    }

    #[test]
    fn a_finished_duplicate_label_is_committed() {
        let body = json!({ "Status": "Label Already Exists", "ExistingJobStatus": "FINISHED" })
            .to_string();

        assert_eq!(
            classify_response(&body, &table_name(), "label-1").unwrap(),
            StreamLoadOutcome::Committed
        );
    }

    #[test]
    fn a_running_duplicate_label_is_still_in_progress() {
        let body =
            json!({ "Status": "Label Already Exists", "ExistingJobStatus": "RUNNING" }).to_string();

        assert_eq!(
            classify_response(&body, &table_name(), "label-1").unwrap(),
            StreamLoadOutcome::InProgress
        );
    }

    #[test]
    fn a_failed_load_reports_the_error_url() {
        let body = json!({
            "Status": "Fail",
            "Message": "too many filtered rows",
            "ErrorURL": "http://127.0.0.1:8040/api/_load_error_log?file=x",
            "NumberFilteredRows": 3
        })
        .to_string();

        let error = classify_response(&body, &table_name(), "label-1").unwrap_err();

        assert_eq!(error.kind(), ErrorKind::DestinationError);
        assert!(error.detail().is_some_and(|detail| detail.contains("filtered_rows=3")));
    }

    #[test]
    fn labels_are_deterministic_for_a_source_position() {
        let first = build_stream_load_label(1, "public_users", 0x1234, 5, 0);
        let second = build_stream_load_label(1, "public_users", 0x1234, 5, 0);

        assert_eq!(first, second);
        assert_eq!(first, "etl_1_public_users_0000000000001234_0000000000000005_0");
        assert_eq!(
            build_copy_stream_load_label(42, "public_orders", 0xabc, 7),
            "etl_copy_42_public_orders_0000000000000abc_0000000000000007"
        );
    }
}
