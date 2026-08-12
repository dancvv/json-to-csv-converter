mod csv_detection;
mod json_stream;

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
pub use csv_detection::{CsvDetection, detect_csv};
use csv_detection::{detect_csv_internal, open_decoded_file};
use json_stream::stream_json_records;
use rust_xlsxwriter::{Color, Format, FormatBorder, Workbook, Worksheet};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use tempfile::{Builder, TempPath};

const EXCEL_MAX_COLUMNS: usize = 16_384;
const EXCEL_MAX_DATA_ROWS: usize = 1_048_575;
const EXCEL_MAX_STRING_LENGTH: usize = 32_767;
const MAX_EXACT_EXCEL_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversionReport {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub rows: usize,
    pub columns: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csv_detection: Option<CsvDetection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressPhase {
    Detecting,
    Scanning,
    Writing,
    Finalizing,
}

pub trait ConversionObserver: Send + Sync {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn on_progress(&self, _phase: ProgressPhase, _rows: usize) {}
}

#[derive(Debug, Default)]
struct NoopObserver;

impl ConversionObserver for NoopObserver {}

fn check_cancelled(observer: &dyn ConversionObserver) -> Result<()> {
    if observer.is_cancelled() {
        bail!("conversion cancelled");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct JsonToCsvOptions {
    pub delimiter: u8,
    pub utf8_bom: bool,
    pub overwrite: bool,
    pub max_rows: Option<usize>,
}

impl Default for JsonToCsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            utf8_bom: false,
            overwrite: false,
            max_rows: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JsonToExcelOptions {
    pub overwrite: bool,
    pub max_rows: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct CsvToJsonOptions {
    pub delimiter: Option<u8>,
    pub encoding: Option<String>,
    pub has_headers: Option<bool>,
    pub infer_types: bool,
    pub pretty: bool,
    pub overwrite: bool,
    pub max_rows: Option<usize>,
}

impl Default for CsvToJsonOptions {
    fn default() -> Self {
        Self {
            delimiter: None,
            encoding: None,
            has_headers: None,
            infer_types: false,
            pretty: true,
            overwrite: false,
            max_rows: None,
        }
    }
}

pub fn json_to_csv(
    input_path: &Path,
    output_path: &Path,
    records_path: Option<&str>,
    options: JsonToCsvOptions,
) -> Result<ConversionReport> {
    json_to_csv_with_observer(
        input_path,
        output_path,
        records_path,
        options,
        &NoopObserver,
    )
}

pub fn json_to_csv_with_observer(
    input_path: &Path,
    output_path: &Path,
    records_path: Option<&str>,
    options: JsonToCsvOptions,
    observer: &dyn ConversionObserver,
) -> Result<ConversionReport> {
    validate_delimiter(options.delimiter)?;
    prepare_paths(input_path, output_path, options.overwrite)?;
    validate_max_rows(options.max_rows)?;
    check_cancelled(observer)?;

    let mut headers = Vec::new();
    let mut seen_headers = HashSet::new();
    let scan = stream_json_records(
        input_path,
        records_path,
        options.max_rows,
        observer,
        ProgressPhase::Scanning,
        |value| {
            let row = flatten_value(value)?;
            for header in row.keys() {
                if seen_headers.insert(header.clone()) {
                    headers.push(header.clone());
                }
            }
            Ok(())
        },
    )?;

    check_cancelled(observer)?;
    let temp_path = temporary_path_for(output_path)?;
    let mut output = BufWriter::new(
        File::create(&temp_path)
            .with_context(|| format!("failed to create output for {}", output_path.display()))?,
    );
    if options.utf8_bom {
        output.write_all(&[0xEF, 0xBB, 0xBF])?;
    }

    let mut writer = csv::WriterBuilder::new()
        .delimiter(options.delimiter)
        .from_writer(output);
    if !headers.is_empty() {
        writer
            .write_record(&headers)
            .context("failed to write the CSV header")?;
    }
    let written = stream_json_records(
        input_path,
        records_path,
        options.max_rows,
        observer,
        ProgressPhase::Writing,
        |value| {
            let row = flatten_value(value)?;
            ensure_known_headers(&row, &seen_headers)?;
            let values = headers
                .iter()
                .map(|header| row.get(header).map(cell_to_text).unwrap_or_default());
            writer
                .write_record(values)
                .context("failed to write a CSV row")
        },
    )?;
    writer.flush().context("failed to flush CSV data")?;
    drop(writer);

    observer.on_progress(ProgressPhase::Finalizing, written.rows);
    check_cancelled(observer)?;
    commit_temp_path(temp_path, output_path, options.overwrite)?;
    Ok(ConversionReport {
        input_path: input_path.to_path_buf(),
        output_path: output_path.to_path_buf(),
        rows: written.rows,
        columns: headers.len(),
        truncated: scan.truncated || written.truncated,
        csv_detection: None,
    })
}

pub fn json_to_excel(
    input_path: &Path,
    output_path: &Path,
    records_path: Option<&str>,
    options: JsonToExcelOptions,
) -> Result<ConversionReport> {
    json_to_excel_with_observer(
        input_path,
        output_path,
        records_path,
        options,
        &NoopObserver,
    )
}

pub fn json_to_excel_with_observer(
    input_path: &Path,
    output_path: &Path,
    records_path: Option<&str>,
    options: JsonToExcelOptions,
    observer: &dyn ConversionObserver,
) -> Result<ConversionReport> {
    prepare_paths(input_path, output_path, options.overwrite)?;
    validate_max_rows(options.max_rows)?;
    check_cancelled(observer)?;

    let mut headers = Vec::new();
    let mut seen_headers = HashSet::new();
    let scan = stream_json_records(
        input_path,
        records_path,
        options.max_rows,
        observer,
        ProgressPhase::Scanning,
        |value| {
            let row = flatten_value(value)?;
            for header in row.keys() {
                if seen_headers.insert(header.clone()) {
                    headers.push(header.clone());
                }
            }
            Ok(())
        },
    )?;

    if headers.len() > EXCEL_MAX_COLUMNS {
        bail!(
            "the table has {} columns, but an Excel worksheet supports at most {EXCEL_MAX_COLUMNS}",
            headers.len()
        );
    }
    if scan.rows > EXCEL_MAX_DATA_ROWS {
        bail!(
            "the table has {} data rows, but an Excel worksheet supports at most {EXCEL_MAX_DATA_ROWS} rows plus the header",
            scan.rows
        );
    }

    check_cancelled(observer)?;
    let mut workbook = Workbook::new();
    let mut widths = headers
        .iter()
        .map(|header| header.chars().count())
        .collect::<Vec<_>>();
    let written;
    {
        let worksheet = workbook.add_worksheet_with_constant_memory();
        worksheet.set_name("Data")?;
        write_excel_headers(worksheet, &headers)?;
        let mut excel_row = 1_u32;
        written = stream_json_records(
            input_path,
            records_path,
            options.max_rows,
            observer,
            ProgressPhase::Writing,
            |value| {
                let row = flatten_value(value)?;
                ensure_known_headers(&row, &seen_headers)?;
                write_excel_row(worksheet, excel_row, &headers, &row, &mut widths)?;
                excel_row += 1;
                Ok(())
            },
        )?;
        finalize_excel_sheet(worksheet, &headers, &widths, written.rows)?;
    }

    observer.on_progress(ProgressPhase::Finalizing, written.rows);
    check_cancelled(observer)?;
    let temp_path = temporary_path_for(output_path)?;
    workbook
        .save(&temp_path)
        .with_context(|| format!("failed to create Excel file at {}", output_path.display()))?;
    check_cancelled(observer)?;
    commit_temp_path(temp_path, output_path, options.overwrite)?;

    Ok(ConversionReport {
        input_path: input_path.to_path_buf(),
        output_path: output_path.to_path_buf(),
        rows: written.rows,
        columns: headers.len(),
        truncated: scan.truncated || written.truncated,
        csv_detection: None,
    })
}

pub fn csv_to_json(
    input_path: &Path,
    output_path: &Path,
    options: CsvToJsonOptions,
) -> Result<ConversionReport> {
    csv_to_json_with_observer(input_path, output_path, options, &NoopObserver)
}

pub fn csv_to_json_with_observer(
    input_path: &Path,
    output_path: &Path,
    options: CsvToJsonOptions,
    observer: &dyn ConversionObserver,
) -> Result<ConversionReport> {
    if let Some(delimiter) = options.delimiter {
        validate_delimiter(delimiter)?;
    }
    prepare_paths(input_path, output_path, options.overwrite)?;
    validate_max_rows(options.max_rows)?;
    check_cancelled(observer)?;
    observer.on_progress(ProgressPhase::Detecting, 0);

    let detected = detect_csv_internal(
        input_path,
        options.encoding.as_deref(),
        options.delimiter,
        options.has_headers,
    )?;
    check_cancelled(observer)?;

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(detected.delimiter)
        .has_headers(false)
        .flexible(false)
        .from_reader(open_decoded_file(input_path, detected.encoding)?);

    let first_record = reader
        .records()
        .next()
        .transpose()
        .context("failed to parse the first CSV record")?
        .with_context(|| format!("CSV input is empty: {}", input_path.display()))?;
    let headers = if detected.report.has_headers {
        let headers = first_record.iter().map(str::to_owned).collect::<Vec<_>>();
        validate_csv_headers(&headers)?;
        headers
    } else {
        (1..=first_record.len())
            .map(|index| format!("column_{index}"))
            .collect::<Vec<_>>()
    };

    let temp_path = temporary_path_for(output_path)?;
    let mut output = BufWriter::new(
        File::create(&temp_path)
            .with_context(|| format!("failed to create output for {}", output_path.display()))?,
    );
    let mut first_output = true;
    write_json_array_start(&mut output, options.pretty)?;
    let mut rows = 0usize;
    let mut truncated = false;

    if !detected.report.has_headers {
        write_csv_record_as_json(
            &mut output,
            &headers,
            &first_record,
            options.infer_types,
            options.pretty,
            &mut first_output,
        )?;
        rows += 1;
        observer.on_progress(ProgressPhase::Writing, rows);
    }

    for (index, record) in reader.records().enumerate() {
        check_cancelled(observer)?;
        if options.max_rows.is_some_and(|limit| rows >= limit) {
            truncated = true;
            break;
        }
        let source_row = index + 2;
        let record = record.with_context(|| format!("failed to parse CSV row {source_row}"))?;
        write_csv_record_as_json(
            &mut output,
            &headers,
            &record,
            options.infer_types,
            options.pretty,
            &mut first_output,
        )?;
        rows += 1;
        observer.on_progress(ProgressPhase::Writing, rows);
    }
    write_json_array_end(&mut output, options.pretty, first_output)?;
    output.flush().context("failed to flush JSON output")?;
    drop(output);

    observer.on_progress(ProgressPhase::Finalizing, rows);
    check_cancelled(observer)?;
    commit_temp_path(temp_path, output_path, options.overwrite)?;

    Ok(ConversionReport {
        input_path: input_path.to_path_buf(),
        output_path: output_path.to_path_buf(),
        rows,
        columns: headers.len(),
        truncated,
        csv_detection: Some(detected.report),
    })
}

pub fn parse_delimiter(value: Option<&str>) -> Result<u8> {
    let value = value.unwrap_or(",");
    let delimiter = match value.to_ascii_lowercase().as_str() {
        "," | "comma" => b',',
        ";" | "semicolon" => b';',
        "\\t" | "tab" => b'\t',
        "|" | "pipe" => b'|',
        _ => {
            let bytes = value.as_bytes();
            if bytes.len() != 1 || !bytes[0].is_ascii() {
                bail!(
                    "delimiter must be one ASCII character or one of: comma, semicolon, tab, pipe"
                );
            }
            bytes[0]
        }
    };
    validate_delimiter(delimiter)?;
    Ok(delimiter)
}

pub fn parse_optional_delimiter(value: Option<&str>) -> Result<Option<u8>> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) if value.eq_ignore_ascii_case("auto") => Ok(None),
        Some(value) => parse_delimiter(Some(value)).map(Some),
    }
}

fn validate_delimiter(delimiter: u8) -> Result<()> {
    if matches!(delimiter, b'\r' | b'\n' | 0) {
        bail!("delimiter cannot be a line break or NUL byte");
    }
    Ok(())
}

fn flatten_value(value: Value) -> Result<Map<String, Value>> {
    let mut row = Map::new();
    match value {
        Value::Object(object) => flatten_object(None, &object, &mut row)?,
        value => {
            row.insert("value".to_owned(), value);
        }
    }
    Ok(row)
}

fn flatten_object(
    prefix: Option<&str>,
    object: &Map<String, Value>,
    output: &mut Map<String, Value>,
) -> Result<()> {
    for (key, value) in object {
        let flattened_key = match prefix {
            Some(prefix) => format!("{prefix}.{key}"),
            None => key.clone(),
        };

        if let Value::Object(child) = value
            && !child.is_empty()
        {
            flatten_object(Some(&flattened_key), child, output)?;
            continue;
        }

        if output
            .insert(flattened_key.clone(), value.clone())
            .is_some()
        {
            bail!(
                "flattening nested JSON produced duplicate column `{flattened_key}`; rename the colliding keys"
            );
        }
    }
    Ok(())
}

fn ensure_known_headers(row: &Map<String, Value>, headers: &HashSet<String>) -> Result<()> {
    if let Some(header) = row.keys().find(|header| !headers.contains(*header)) {
        bail!(
            "JSON input changed while it was being converted: new column `{header}` appeared during the second streaming pass"
        );
    }
    Ok(())
}

fn write_excel_headers(worksheet: &mut Worksheet, headers: &[String]) -> Result<()> {
    let header_format = Format::new()
        .set_bold()
        .set_font_color(Color::White)
        .set_background_color(Color::RGB(0x1F4E78))
        .set_border(FormatBorder::Thin);

    for (column, header) in headers.iter().enumerate() {
        ensure_excel_string_length(header, "header")?;
        worksheet.write_string_with_format(0, column as u16, header, &header_format)?;
    }
    Ok(())
}

fn write_excel_row(
    worksheet: &mut Worksheet,
    excel_row: u32,
    headers: &[String],
    row: &Map<String, Value>,
    widths: &mut [usize],
) -> Result<()> {
    for (column, header) in headers.iter().enumerate() {
        if let Some(value) = row.get(header) {
            let width = cell_to_text(value).chars().count();
            widths[column] = widths[column].max(width);
            write_excel_value(worksheet, excel_row, column as u16, value)?;
        }
    }
    Ok(())
}

fn finalize_excel_sheet(
    worksheet: &mut Worksheet,
    headers: &[String],
    widths: &[usize],
    rows: usize,
) -> Result<()> {
    if !headers.is_empty() {
        worksheet.set_freeze_panes(1, 0)?;
        worksheet.autofilter(0, 0, rows as u32, (headers.len() - 1) as u16)?;

        for (column, max_width) in widths.iter().enumerate() {
            let width = (*max_width as f64 + 2.0).clamp(10.0, 50.0);
            worksheet.set_column_width(column as u16, width)?;
        }
    }

    Ok(())
}

fn write_csv_record_as_json(
    output: &mut impl Write,
    headers: &[String],
    record: &csv::StringRecord,
    infer_types: bool,
    pretty: bool,
    first_output: &mut bool,
) -> Result<()> {
    let mut row = Map::with_capacity(headers.len());
    for (header, value) in headers.iter().zip(record.iter()) {
        row.insert(
            header.clone(),
            if infer_types {
                infer_json_value(value)
            } else {
                Value::String(value.to_owned())
            },
        );
    }

    if !*first_output {
        output.write_all(if pretty { b",\n" } else { b"," })?;
    }
    if pretty {
        serde_json::to_writer_pretty(&mut *output, &Value::Object(row))?;
    } else {
        serde_json::to_writer(&mut *output, &Value::Object(row))?;
    }
    *first_output = false;
    Ok(())
}

fn write_json_array_start(output: &mut impl Write, pretty: bool) -> Result<()> {
    output.write_all(if pretty { b"[\n" } else { b"[" })?;
    Ok(())
}

fn write_json_array_end(output: &mut impl Write, pretty: bool, empty: bool) -> Result<()> {
    match (pretty, empty) {
        (true, true) => output.write_all(b"]\n")?,
        (true, false) => output.write_all(b"\n]\n")?,
        (false, _) => output.write_all(b"]\n")?,
    }
    Ok(())
}

fn validate_max_rows(max_rows: Option<usize>) -> Result<()> {
    if max_rows == Some(0) {
        bail!("max_rows must be greater than zero");
    }
    Ok(())
}

fn write_excel_value(
    worksheet: &mut Worksheet,
    row: u32,
    column: u16,
    value: &Value,
) -> Result<()> {
    match value {
        Value::Null => {}
        Value::Bool(value) => {
            worksheet.write_boolean(row, column, *value)?;
        }
        Value::Number(value) => write_excel_number(worksheet, row, column, value)?,
        Value::String(value) => {
            ensure_excel_string_length(value, "cell")?;
            worksheet.write_string(row, column, value)?;
        }
        Value::Array(_) | Value::Object(_) => {
            let value = serde_json::to_string(value)?;
            ensure_excel_string_length(&value, "cell")?;
            worksheet.write_string(row, column, &value)?;
        }
    }
    Ok(())
}

fn write_excel_number(
    worksheet: &mut Worksheet,
    row: u32,
    column: u16,
    number: &Number,
) -> Result<()> {
    if let Some(value) = number.as_i64() {
        if value.unsigned_abs() <= MAX_EXACT_EXCEL_INTEGER {
            worksheet.write_number(row, column, value as f64)?;
        } else {
            worksheet.write_string(row, column, value.to_string())?;
        }
    } else if let Some(value) = number.as_u64() {
        if value <= MAX_EXACT_EXCEL_INTEGER {
            worksheet.write_number(row, column, value as f64)?;
        } else {
            worksheet.write_string(row, column, value.to_string())?;
        }
    } else if let Some(value) = number.as_f64() {
        worksheet.write_number(row, column, value)?;
    }
    Ok(())
}

fn ensure_excel_string_length(value: &str, location: &str) -> Result<()> {
    let length = value.chars().count();
    if length > EXCEL_MAX_STRING_LENGTH {
        bail!(
            "an Excel {location} contains {length} characters, exceeding Excel's {EXCEL_MAX_STRING_LENGTH}-character cell limit"
        );
    }
    Ok(())
}

fn validate_csv_headers(headers: &[String]) -> Result<()> {
    if headers.is_empty() {
        bail!("CSV input must contain a header row");
    }

    let mut seen = HashSet::new();
    for header in headers {
        if header.is_empty() {
            bail!("CSV headers cannot be empty");
        }
        if !seen.insert(header) {
            bail!("CSV header `{header}` occurs more than once");
        }
    }
    Ok(())
}

fn infer_json_value(value: &str) -> Value {
    match value {
        "null" => return Value::Null,
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }

    if let Ok(number) = value.parse::<i64>()
        && number.to_string() == value
    {
        return Value::Number(number.into());
    }
    if let Ok(number) = value.parse::<u64>()
        && number.to_string() == value
    {
        return Value::Number(number.into());
    }
    if (value.contains('.') || value.contains('e') || value.contains('E'))
        && let Ok(number) = value.parse::<f64>()
        && let Some(number) = Number::from_f64(number)
    {
        return Value::Number(number);
    }

    Value::String(value.to_owned())
}

fn cell_to_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn prepare_paths(input_path: &Path, output_path: &Path, overwrite: bool) -> Result<()> {
    if !input_path.is_file() {
        bail!("input file does not exist: {}", input_path.display());
    }

    let parent = output_parent(output_path);
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create output directory {}",
            parent.to_string_lossy()
        )
    })?;

    let input_canonical = fs::canonicalize(input_path)
        .with_context(|| format!("failed to resolve input path {}", input_path.display()))?;
    let output_canonical = if output_path.exists() {
        Some(
            fs::canonicalize(output_path).with_context(|| {
                format!("failed to resolve output path {}", output_path.display())
            })?,
        )
    } else {
        let parent = fs::canonicalize(parent)
            .with_context(|| format!("failed to resolve output directory {}", parent.display()))?;
        output_path.file_name().map(|name| parent.join(name))
    };

    if output_canonical.as_ref() == Some(&input_canonical) {
        bail!("input and output paths must be different");
    }
    if output_path.exists() && !overwrite {
        bail!(
            "output file already exists: {} (set overwrite=true to replace it)",
            output_path.display()
        );
    }
    if output_path.is_dir() {
        bail!("output path is a directory: {}", output_path.display());
    }
    Ok(())
}

fn output_parent(output_path: &Path) -> &Path {
    output_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn temporary_path_for(output_path: &Path) -> Result<TempPath> {
    Builder::new()
        .prefix(".json-to-csv-converter-")
        .tempfile_in(output_parent(output_path))
        .map(|file| file.into_temp_path())
        .with_context(|| {
            format!(
                "failed to create a temporary file beside {}",
                output_path.display()
            )
        })
}

fn commit_temp_path(temp_path: TempPath, output_path: &Path, overwrite: bool) -> Result<()> {
    let result = if overwrite {
        temp_path.persist(output_path)
    } else {
        temp_path.persist_noclobber(output_path)
    };
    result
        .map(|_| ())
        .with_context(|| format!("failed to save output file {}", output_path.display()))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{Read, Write},
        sync::atomic::{AtomicBool, Ordering},
    };

    use encoding_rs::GBK;
    use serde_json::json;
    use tempfile::tempdir;
    use zip::ZipArchive;

    use super::*;

    #[test]
    fn converts_nested_json_to_csv_with_union_headers_and_escaping() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("people.json");
        let output = directory.path().join("people.csv");
        fs::write(
            &input,
            serde_json::to_vec(&json!([
                {
                    "name": "张三, Jr.",
                    "active": true,
                    "address": {"city": "上海"},
                    "tags": ["研发", "AI"]
                },
                {"name": "李四", "score": 98.5}
            ]))
            .unwrap(),
        )
        .unwrap();

        let report = json_to_csv(
            &input,
            &output,
            None,
            JsonToCsvOptions {
                utf8_bom: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.rows, 2);
        assert_eq!(report.columns, 5);
        let bytes = fs::read(&output).unwrap();
        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
        let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
        assert!(text.starts_with("name,active,address.city,tags,score"));
        assert!(text.contains("\"张三, Jr.\""));
        assert!(text.contains("\"[\"\"研发\"\",\"\"AI\"\"]\""));
    }

    #[test]
    fn selects_records_with_dot_path_and_json_pointer() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("wrapped.json");
        fs::write(&input, r#"{"payload":{"items":[{"id":1},{"id":2}]}}"#).unwrap();

        for (index, records_path) in ["payload.items", "/payload/items"].iter().enumerate() {
            let output = directory.path().join(format!("items-{index}.csv"));
            let report = json_to_csv(
                &input,
                &output,
                Some(records_path),
                JsonToCsvOptions::default(),
            )
            .unwrap();
            assert_eq!(report.rows, 2);
            assert_eq!(fs::read_to_string(output).unwrap(), "id\n1\n2\n");
        }
    }

    #[test]
    fn accepts_utf8_bom_in_json_and_csv_inputs() {
        let directory = tempdir().unwrap();
        let json_input = directory.path().join("bom.json");
        let csv_output = directory.path().join("bom.csv");
        fs::write(&json_input, b"\xEF\xBB\xBF[{\"id\":1}]").unwrap();
        json_to_csv(&json_input, &csv_output, None, JsonToCsvOptions::default()).unwrap();
        assert_eq!(fs::read_to_string(&csv_output).unwrap(), "id\n1\n");

        let csv_input = directory.path().join("input-bom.csv");
        let json_output = directory.path().join("input-bom.json");
        fs::write(&csv_input, b"\xEF\xBB\xBFid,name\n001,Alice\n").unwrap();
        csv_to_json(&csv_input, &json_output, CsvToJsonOptions::default()).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(json_output).unwrap()).unwrap();
        assert_eq!(value[0]["id"], "001");
        assert_eq!(value[0]["name"], "Alice");
    }

    #[test]
    fn converts_csv_to_json_without_losing_strings_by_default() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("people.csv");
        let output = directory.path().join("people.json");
        fs::write(
            &input,
            "id,name,note\n001,张三,\"line 1\nline 2\"\n002,李四,\"a,b\"\n",
        )
        .unwrap();

        let report = csv_to_json(&input, &output, CsvToJsonOptions::default()).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();

        assert_eq!(report.rows, 2);
        assert_eq!(value[0]["id"], "001");
        assert_eq!(value[0]["note"], "line 1\nline 2");
        assert_eq!(value[1]["note"], "a,b");
    }

    #[test]
    fn optionally_infers_unambiguous_csv_types() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("values.csv");
        let output = directory.path().join("values.json");
        fs::write(
            &input,
            "integer,float,boolean,null_value,leading_zero\n42,1.5,true,null,001\n",
        )
        .unwrap();

        csv_to_json(
            &input,
            &output,
            CsvToJsonOptions {
                infer_types: true,
                ..Default::default()
            },
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();

        assert_eq!(value[0]["integer"], 42);
        assert_eq!(value[0]["float"], 1.5);
        assert_eq!(value[0]["boolean"], true);
        assert!(value[0]["null_value"].is_null());
        assert_eq!(value[0]["leading_zero"], "001");
    }

    #[test]
    fn creates_a_real_xlsx_workbook() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("people.json");
        let output = directory.path().join("people.xlsx");
        fs::write(
            &input,
            r#"[{"name":"张三","age":30,"active":true},{"name":"李四","age":31,"active":false}]"#,
        )
        .unwrap();

        let report = json_to_excel(&input, &output, None, JsonToExcelOptions::default()).unwrap();
        assert_eq!(report.rows, 2);
        assert_eq!(report.columns, 3);

        let file = File::open(output).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        assert!(archive.by_name("xl/workbook.xml").is_ok());
        let mut worksheet = String::new();
        archive
            .by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut worksheet)
            .unwrap();
        assert!(worksheet.contains("<autoFilter"));
        assert!(worksheet.contains("<pane"));
    }

    #[test]
    fn refuses_to_overwrite_an_existing_file_by_default() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("input.json");
        let output = directory.path().join("output.csv");
        fs::write(&input, "[]").unwrap();
        fs::write(&output, "keep me").unwrap();

        let error = json_to_csv(&input, &output, None, JsonToCsvOptions::default())
            .unwrap_err()
            .to_string();

        assert!(error.contains("already exists"));
        assert_eq!(fs::read_to_string(output).unwrap(), "keep me");
    }

    #[test]
    fn rejects_duplicate_csv_headers() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("duplicate.csv");
        let output = directory.path().join("duplicate.json");
        fs::write(&input, "name,name\nAlice,Bob\n").unwrap();

        let error = csv_to_json(&input, &output, CsvToJsonOptions::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("occurs more than once"));
    }

    #[test]
    fn automatically_converts_semicolon_gbk_csv() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("people-gbk.csv");
        let output = directory.path().join("people-gbk.json");
        let (bytes, _, _) = GBK.encode("编号;姓名;城市\n001;张三;上海\n002;李四;北京\n");
        fs::write(&input, bytes).unwrap();

        let report = csv_to_json(&input, &output, CsvToJsonOptions::default()).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();

        let detection = report.csv_detection.unwrap();
        assert_eq!(detection.encoding, "GBK");
        assert_eq!(detection.delimiter, ";");
        assert!(detection.has_headers);
        assert_eq!(value[0]["编号"], "001");
        assert_eq!(value[1]["城市"], "北京");
    }

    #[test]
    fn automatically_generates_columns_for_headerless_tab_data() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("headerless.tsv");
        let output = directory.path().join("headerless.json");
        fs::write(&input, "1\tAlice\n2\tBob\n").unwrap();

        let report = csv_to_json(&input, &output, CsvToJsonOptions::default()).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();

        assert!(!report.csv_detection.unwrap().has_headers);
        assert_eq!(value[0]["column_1"], "1");
        assert_eq!(value[0]["column_2"], "Alice");
        assert_eq!(value[1]["column_1"], "2");
    }

    #[test]
    fn max_rows_truncates_streaming_json_and_csv_conversions() {
        let directory = tempdir().unwrap();
        let json_input = directory.path().join("large.json");
        let csv_output = directory.path().join("limited.csv");
        let mut json = File::create(&json_input).unwrap();
        json.write_all(b"[").unwrap();
        for index in 0..5_000 {
            if index > 0 {
                json.write_all(b",").unwrap();
            }
            write!(json, "{{\"id\":{index},\"name\":\"record-{index}\"}}").unwrap();
        }
        json.write_all(b"]").unwrap();
        drop(json);

        let json_report = json_to_csv(
            &json_input,
            &csv_output,
            None,
            JsonToCsvOptions {
                max_rows: Some(125),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(json_report.rows, 125);
        assert!(json_report.truncated);

        let json_output = directory.path().join("limited.json");
        let csv_report = csv_to_json(
            &csv_output,
            &json_output,
            CsvToJsonOptions {
                max_rows: Some(20),
                ..Default::default()
            },
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&json_output).unwrap()).unwrap();
        assert_eq!(csv_report.rows, 20);
        assert!(csv_report.truncated);
        assert_eq!(value.as_array().unwrap().len(), 20);
    }

    #[derive(Debug)]
    struct CancelDuringWrite {
        cancelled: AtomicBool,
    }

    impl ConversionObserver for CancelDuringWrite {
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::Relaxed)
        }

        fn on_progress(&self, phase: ProgressPhase, rows: usize) {
            if phase == ProgressPhase::Writing && rows >= 25 {
                self.cancelled.store(true, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn cancellation_removes_partial_output() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("records.json");
        let output = directory.path().join("records.csv");
        let records = (0..200)
            .map(|index| json!({"id": index, "value": format!("value-{index}")}))
            .collect::<Vec<_>>();
        fs::write(&input, serde_json::to_vec(&records).unwrap()).unwrap();

        let observer = CancelDuringWrite {
            cancelled: AtomicBool::new(false),
        };
        let error = json_to_csv_with_observer(
            &input,
            &output,
            None,
            JsonToCsvOptions::default(),
            &observer,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("cancelled"));
        assert!(!output.exists());
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".json-to-csv-converter-")
        }));
    }
}
