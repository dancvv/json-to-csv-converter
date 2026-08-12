use std::{
    env,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use json_to_csv_converter_core::{
    ConversionObserver, ConversionReport, CsvToJsonOptions, JsonToCsvOptions, JsonToExcelOptions,
    ProgressPhase, csv_to_json_with_observer, detect_csv, json_to_csv_with_observer,
    json_to_excel_with_observer, parse_delimiter, parse_optional_delimiter,
};
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{Implementation, ProgressNotificationParam, ServerCapabilities, ServerInfo},
    schemars::{self, JsonSchema},
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

mod cli;

const BASE_DIRECTORY_ENV: &str = "JSON_TO_CSV_CONVERTER_BASE_DIR";

#[derive(Debug, Clone)]
struct JsonToCsvConverter;

#[derive(Debug, Deserialize, JsonSchema)]
struct JsonToCsvRequest {
    /// Absolute or base-directory-relative path to the input JSON file.
    input_path: String,
    /// Optional output path. Defaults to the input path with a .csv extension.
    output_path: Option<String>,
    /// Optional dot path (payload.items) or JSON Pointer (/payload/items) to the records.
    records_path: Option<String>,
    /// CSV delimiter: comma, semicolon, tab, pipe, or one ASCII character. Defaults to comma.
    delimiter: Option<String>,
    /// Add a UTF-8 BOM for compatibility with older spreadsheet applications. Defaults to false.
    utf8_bom: Option<bool>,
    /// Replace an existing output file. Defaults to false.
    overwrite: Option<bool>,
    /// Stop after this many records and mark the result as truncated. By default all records are converted.
    max_rows: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct JsonToExcelRequest {
    /// Absolute or base-directory-relative path to the input JSON file.
    input_path: String,
    /// Optional output path. Defaults to the input path with an .xlsx extension.
    output_path: Option<String>,
    /// Optional dot path (payload.items) or JSON Pointer (/payload/items) to the records.
    records_path: Option<String>,
    /// Replace an existing output file. Defaults to false.
    overwrite: Option<bool>,
    /// Stop after this many records and mark the result as truncated. By default all records are converted.
    max_rows: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CsvToJsonRequest {
    /// Absolute or base-directory-relative path to the input CSV file.
    input_path: String,
    /// Optional output path. Defaults to the input path with a .json extension.
    output_path: Option<String>,
    /// CSV delimiter: auto, comma, semicolon, tab, pipe, or one ASCII character. Defaults to auto detection.
    delimiter: Option<String>,
    /// Source character encoding label such as utf-8, utf-16le, gbk, big5, or windows-1252. Defaults to auto detection.
    encoding: Option<String>,
    /// Whether the first CSV record is a header. Defaults to auto detection.
    has_headers: Option<bool>,
    /// Infer null, boolean, and unambiguous numeric values. Defaults to false to preserve data such as 001.
    infer_types: Option<bool>,
    /// Pretty-print JSON output. Defaults to true.
    pretty: Option<bool>,
    /// Replace an existing output file. Defaults to false.
    overwrite: Option<bool>,
    /// Stop after this many data records and mark the result as truncated. By default all records are converted.
    max_rows: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DetectCsvRequest {
    /// Absolute or base-directory-relative path to the CSV file.
    input_path: String,
    /// Optional source encoding override. Defaults to auto detection.
    encoding: Option<String>,
    /// Optional delimiter override. Defaults to auto detection.
    delimiter: Option<String>,
    /// Optional header override. Defaults to auto detection.
    has_headers: Option<bool>,
}

#[tool_router]
impl JsonToCsvConverter {
    #[tool(
        description = "Stream a JSON file to CSV without loading all records into memory. JSON arrays become rows, nested objects are flattened with dotted column names, and arrays remain JSON text. Use records_path when rows are nested inside a wrapper object. Reports progress, honors MCP cancellation, and supports max_rows."
    )]
    async fn json_to_csv(
        &self,
        Parameters(request): Parameters<JsonToCsvRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let input = resolve_path(&request.input_path)?;
        let output = resolve_output_path(request.output_path.as_deref(), &input, "csv")?;
        let delimiter = parse_delimiter(request.delimiter.as_deref()).map_err(error_text)?;
        let records_path = request.records_path;
        let max_rows = request.max_rows;
        let report = run_conversion(context, max_rows, "JSON → CSV", move |observer| {
            json_to_csv_with_observer(
                &input,
                &output,
                records_path.as_deref(),
                JsonToCsvOptions {
                    delimiter,
                    utf8_bom: request.utf8_bom.unwrap_or(false),
                    overwrite: request.overwrite.unwrap_or(false),
                    max_rows,
                },
                observer.as_ref(),
            )
            .map_err(error_text)
        })
        .await?;
        format_report("json_to_csv", &report)
    }

    #[tool(
        description = "Stream a JSON file to a real Excel .xlsx workbook without retaining all records in memory. The sheet includes a styled header, frozen header row, autofilter, typed number/boolean cells, and adjusted column widths. Use records_path when rows are nested inside a wrapper object. Reports progress, honors MCP cancellation, and supports max_rows."
    )]
    async fn json_to_excel(
        &self,
        Parameters(request): Parameters<JsonToExcelRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let input = resolve_path(&request.input_path)?;
        let output = resolve_output_path(request.output_path.as_deref(), &input, "xlsx")?;
        let records_path = request.records_path;
        let max_rows = request.max_rows;
        let report = run_conversion(context, max_rows, "JSON → Excel", move |observer| {
            json_to_excel_with_observer(
                &input,
                &output,
                records_path.as_deref(),
                JsonToExcelOptions {
                    overwrite: request.overwrite.unwrap_or(false),
                    max_rows,
                },
                observer.as_ref(),
            )
            .map_err(error_text)
        })
        .await?;
        format_report("json_to_excel", &report)
    }

    #[tool(
        description = "Stream a CSV file to a JSON array without loading the entire file into memory. Encoding, delimiter, and header presence are detected automatically unless overridden. Values remain strings by default so IDs and leading zeros are preserved. Reports progress, honors MCP cancellation, and supports max_rows."
    )]
    async fn csv_to_json(
        &self,
        Parameters(request): Parameters<CsvToJsonRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let input = resolve_path(&request.input_path)?;
        let output = resolve_output_path(request.output_path.as_deref(), &input, "json")?;
        let delimiter =
            parse_optional_delimiter(request.delimiter.as_deref()).map_err(error_text)?;
        let max_rows = request.max_rows;
        let report = run_conversion(context, max_rows, "CSV → JSON", move |observer| {
            csv_to_json_with_observer(
                &input,
                &output,
                CsvToJsonOptions {
                    delimiter,
                    encoding: request.encoding,
                    has_headers: request.has_headers,
                    infer_types: request.infer_types.unwrap_or(false),
                    pretty: request.pretty.unwrap_or(true),
                    overwrite: request.overwrite.unwrap_or(false),
                    max_rows,
                },
                observer.as_ref(),
            )
            .map_err(error_text)
        })
        .await?;
        format_report("csv_to_json", &report)
    }

    #[tool(
        description = "Inspect a CSV file without converting it. Detects character encoding, delimiter, and whether the first record is a header from a bounded sample, so it is safe for large files."
    )]
    fn detect_csv_format(
        &self,
        Parameters(request): Parameters<DetectCsvRequest>,
    ) -> Result<String, String> {
        let input = resolve_path(&request.input_path)?;
        let delimiter =
            parse_optional_delimiter(request.delimiter.as_deref()).map_err(error_text)?;
        let detection = detect_csv(
            &input,
            request.encoding.as_deref(),
            delimiter,
            request.has_headers,
        )
        .map_err(error_text)?;
        serde_json::to_string_pretty(&detection).map_err(error_text)
    }
}

#[tool_handler]
impl ServerHandler for JsonToCsvConverter {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("JSON to CSV Converter")
                    .with_description("Convert JSON to CSV or Excel and CSV to JSON, with automatic CSV format detection, large-file streaming, progress, row limits, and cancellation."),
            )
            .with_instructions(
                "Convert JSON files to CSV or Excel and CSV files to JSON using bounded-memory streaming. CSV encoding, delimiter, and header presence are detected automatically unless overridden. Conversion tools report progress, support max_rows, honor request cancellation, and refuse to overwrite existing outputs unless overwrite=true. Prefer absolute paths unless a base directory is configured."
                    .to_owned(),
            )
    }
}

fn resolve_path(value: &str) -> Result<PathBuf, String> {
    if value.trim().is_empty() {
        return Err("path cannot be empty".to_owned());
    }

    let path = PathBuf::from(value);
    if path.is_absolute() {
        return Ok(path);
    }

    let base = match env::var_os(BASE_DIRECTORY_ENV) {
        Some(base) if !base.is_empty() => PathBuf::from(base),
        _ => env::current_dir().map_err(error_text)?,
    };
    Ok(base.join(path))
}

fn resolve_output_path(
    requested_path: Option<&str>,
    input_path: &std::path::Path,
    extension: &str,
) -> Result<PathBuf, String> {
    match requested_path {
        Some(path) => resolve_path(path),
        None => Ok(input_path.with_extension(extension)),
    }
}

fn format_report(operation: &str, report: &ConversionReport) -> Result<String, String> {
    serde_json::to_string_pretty(&serde_json::json!({
        "ok": true,
        "operation": operation,
        "input_path": report.input_path,
        "output_path": report.output_path,
        "rows": report.rows,
        "columns": report.columns,
        "truncated": report.truncated,
        "csv_detection": report.csv_detection,
    }))
    .map_err(error_text)
}

#[derive(Debug)]
struct ProgressState {
    cancellation: CancellationToken,
    phase: AtomicU8,
    phase_rows: AtomicU64,
    scanned_rows: AtomicU64,
    progress: AtomicU64,
}

impl ProgressState {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            phase: AtomicU8::new(phase_number(ProgressPhase::Detecting)),
            phase_rows: AtomicU64::new(0),
            scanned_rows: AtomicU64::new(0),
            progress: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> (ProgressPhase, u64, u64) {
        (
            number_phase(self.phase.load(Ordering::Relaxed)),
            self.phase_rows.load(Ordering::Relaxed),
            self.progress.load(Ordering::Relaxed),
        )
    }
}

impl ConversionObserver for ProgressState {
    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    fn on_progress(&self, phase: ProgressPhase, rows: usize) {
        let rows = rows as u64;
        self.phase.store(phase_number(phase), Ordering::Relaxed);
        self.phase_rows.store(rows, Ordering::Relaxed);

        let progress = match phase {
            ProgressPhase::Detecting => self.progress.load(Ordering::Relaxed),
            ProgressPhase::Scanning => {
                self.scanned_rows.store(rows, Ordering::Relaxed);
                rows
            }
            ProgressPhase::Writing => self.scanned_rows.load(Ordering::Relaxed) + rows,
            ProgressPhase::Finalizing => self.progress.load(Ordering::Relaxed),
        };
        self.progress.fetch_max(progress, Ordering::Relaxed);
    }
}

async fn run_conversion<F>(
    context: RequestContext<RoleServer>,
    _max_rows: Option<usize>,
    operation: &'static str,
    conversion: F,
) -> Result<ConversionReport, String>
where
    F: FnOnce(Arc<ProgressState>) -> Result<ConversionReport, String> + Send + 'static,
{
    let progress_token = context.meta.get_progress_token();
    let peer = context.peer.clone();
    let state = Arc::new(ProgressState::new(context.ct.clone()));
    let worker_state = Arc::clone(&state);
    let mut worker = tokio::task::spawn_blocking(move || conversion(worker_state));
    let mut interval = tokio::time::interval(Duration::from_millis(400));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let joined = loop {
        tokio::select! {
            result = &mut worker => break result,
            _ = interval.tick() => {
                if let Some(token) = progress_token.clone() {
                    let (phase, rows, progress) = state.snapshot();
                    let message = format!(
                        "{operation}: {} ({rows} rows)",
                        phase_label(phase),
                    );
                    let _ = peer
                        .notify_progress(
                            ProgressNotificationParam::new(token, progress as f64)
                                .with_message(message),
                        )
                        .await;
                }
            }
        }
    };

    let result = joined.map_err(|error| format!("conversion worker failed: {error}"))?;
    if let Some(token) = progress_token {
        let (_, rows, progress) = state.snapshot();
        let status = if result.is_ok() {
            "complete"
        } else {
            "stopped"
        };
        let _ = peer
            .notify_progress(
                ProgressNotificationParam::new(token, progress as f64)
                    .with_message(format!("{operation}: {status} ({rows} rows)")),
            )
            .await;
    }
    result
}

const fn phase_number(phase: ProgressPhase) -> u8 {
    match phase {
        ProgressPhase::Detecting => 0,
        ProgressPhase::Scanning => 1,
        ProgressPhase::Writing => 2,
        ProgressPhase::Finalizing => 3,
    }
}

const fn number_phase(phase: u8) -> ProgressPhase {
    match phase {
        1 => ProgressPhase::Scanning,
        2 => ProgressPhase::Writing,
        3 => ProgressPhase::Finalizing,
        _ => ProgressPhase::Detecting,
    }
}

const fn phase_label(phase: ProgressPhase) -> &'static str {
    match phase {
        ProgressPhase::Detecting => "detecting CSV format",
        ProgressPhase::Scanning => "scanning columns",
        ProgressPhase::Writing => "writing output",
        ProgressPhase::Finalizing => "finalizing file",
    }
}

fn error_text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if cli::handle(&arguments)? {
        return Ok(());
    }

    let service = JsonToCsvConverter.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_default_output_extensions() {
        let input = PathBuf::from("/tmp/example.data.json");
        assert_eq!(
            resolve_output_path(None, &input, "csv").unwrap(),
            PathBuf::from("/tmp/example.data.csv")
        );
        assert_eq!(
            resolve_output_path(None, &input, "xlsx").unwrap(),
            PathBuf::from("/tmp/example.data.xlsx")
        );
    }

    #[test]
    fn progress_is_monotonic_across_two_streaming_passes() {
        let cancellation = CancellationToken::new();
        let progress = ProgressState::new(cancellation.clone());

        progress.on_progress(ProgressPhase::Scanning, 50);
        assert_eq!(progress.snapshot(), (ProgressPhase::Scanning, 50, 50));
        progress.on_progress(ProgressPhase::Writing, 10);
        assert_eq!(progress.snapshot(), (ProgressPhase::Writing, 10, 60));
        progress.on_progress(ProgressPhase::Finalizing, 10);
        assert_eq!(progress.snapshot(), (ProgressPhase::Finalizing, 10, 60));

        cancellation.cancel();
        assert!(progress.is_cancelled());
    }
}
