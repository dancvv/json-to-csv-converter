use std::{
    env,
    path::{Path, PathBuf},
};

use json_to_csv_converter_core::{
    ConversionObserver, ConversionReport, CsvToJsonOptions, JsonToCsvOptions, JsonToExcelOptions,
    ProgressPhase, csv_to_json_with_observer, detect_csv, json_to_csv_with_observer,
    json_to_excel_with_observer, parse_delimiter, parse_optional_delimiter,
};

use crate::BASE_DIRECTORY_ENV;

const JSON_TO_CSV_FLAGS: &[&str] = &[
    "--output",
    "--records-path",
    "--delimiter",
    "--utf8-bom",
    "--overwrite",
    "--max-rows",
];
const JSON_TO_EXCEL_FLAGS: &[&str] = &["--output", "--records-path", "--overwrite", "--max-rows"];
const CSV_TO_JSON_FLAGS: &[&str] = &[
    "--output",
    "--delimiter",
    "--encoding",
    "--has-headers",
    "--infer-types",
    "--compact",
    "--overwrite",
    "--max-rows",
];
const DETECT_CSV_FLAGS: &[&str] = &["--delimiter", "--encoding", "--has-headers"];
const SMART_CONVERT_FLAGS: &[&str] = &[
    "--output",
    "--records-path",
    "--delimiter",
    "--encoding",
    "--has-headers",
    "--infer-types",
    "--compact",
    "--utf8-bom",
    "--overwrite",
    "--max-rows",
];

#[derive(Debug, Default, PartialEq, Eq)]
struct CliOptions {
    input: Option<String>,
    output: Option<String>,
    records_path: Option<String>,
    delimiter: Option<String>,
    encoding: Option<String>,
    has_headers: Option<bool>,
    infer_types: bool,
    compact: bool,
    utf8_bom: bool,
    overwrite: bool,
    max_rows: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Json,
    Csv,
}

pub fn handle(arguments: &[String]) -> Result<bool, String> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Ok(false);
    };

    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        print_help();
        return Ok(true);
    }

    match command {
        "--version" | "-V" if arguments.len() == 1 => {
            println!("json-to-csv-converter-mcp {}", env!("CARGO_PKG_VERSION"));
        }
        "convert" => run_smart_convert(parse_options(&arguments[1..], SMART_CONVERT_FLAGS)?)?,
        "json-to-csv" => run_json_to_csv(parse_options(&arguments[1..], JSON_TO_CSV_FLAGS)?)?,
        "json-to-excel" => run_json_to_excel(parse_options(&arguments[1..], JSON_TO_EXCEL_FLAGS)?)?,
        "csv-to-json" => run_csv_to_json(parse_options(&arguments[1..], CSV_TO_JSON_FLAGS)?)?,
        "detect-csv" => run_detect_csv(parse_options(&arguments[1..], DETECT_CSV_FLAGS)?)?,
        _ => {
            return Err(format!(
                "unknown command `{command}`; run `json-to-csv-converter-mcp --help` for usage"
            ));
        }
    }

    Ok(true)
}

fn parse_options(arguments: &[String], allowed_flags: &[&str]) -> Result<CliOptions, String> {
    let mut options = CliOptions::default();
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        if !argument.starts_with("--") {
            if options.input.replace(argument.clone()).is_some() {
                return Err(format!("unexpected extra input path `{argument}`"));
            }
            index += 1;
            continue;
        }

        if !allowed_flags.contains(&argument.as_str()) {
            return Err(format!("option `{argument}` is not valid for this command"));
        }

        match argument.as_str() {
            "--output" => options.output = Some(take_value(arguments, &mut index, argument)?),
            "--records-path" => {
                options.records_path = Some(take_value(arguments, &mut index, argument)?)
            }
            "--delimiter" => options.delimiter = Some(take_value(arguments, &mut index, argument)?),
            "--encoding" => options.encoding = Some(take_value(arguments, &mut index, argument)?),
            "--has-headers" => {
                let value = take_value(arguments, &mut index, argument)?;
                options.has_headers = parse_header_mode(&value)?;
            }
            "--max-rows" => {
                let value = take_value(arguments, &mut index, argument)?;
                let rows = value.parse::<usize>().map_err(|_| {
                    format!("`--max-rows` must be a positive integer, got `{value}`")
                })?;
                if rows == 0 {
                    return Err("`--max-rows` must be greater than zero".to_owned());
                }
                options.max_rows = Some(rows);
            }
            "--infer-types" => {
                options.infer_types = true;
                index += 1;
            }
            "--compact" => {
                options.compact = true;
                index += 1;
            }
            "--utf8-bom" => {
                options.utf8_bom = true;
                index += 1;
            }
            "--overwrite" => {
                options.overwrite = true;
                index += 1;
            }
            _ => unreachable!("allowed CLI flag was not handled"),
        }
    }

    if options.input.as_deref().is_none_or(str::is_empty) {
        return Err("an input file path is required".to_owned());
    }
    Ok(options)
}

fn take_value(arguments: &[String], index: &mut usize, option: &str) -> Result<String, String> {
    let value = arguments
        .get(*index + 1)
        .ok_or_else(|| format!("option `{option}` requires a value"))?
        .clone();
    *index += 2;
    Ok(value)
}

fn parse_header_mode(value: &str) -> Result<Option<bool>, String> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(None),
        "true" | "yes" => Ok(Some(true)),
        "false" | "no" => Ok(Some(false)),
        _ => Err(format!(
            "`--has-headers` must be auto, true, or false, got `{value}`"
        )),
    }
}

fn run_smart_convert(options: CliOptions) -> Result<(), String> {
    let input = required_input(&options)?;
    match input_kind(&input)? {
        InputKind::Json => run_json_to_csv(options),
        InputKind::Csv => run_csv_to_json(options),
    }
}

fn run_json_to_csv(options: CliOptions) -> Result<(), String> {
    let input = required_input(&options)?;
    let output = output_path(options.output.as_deref(), &input, "csv")?;
    let delimiter = parse_delimiter(options.delimiter.as_deref()).map_err(error_text)?;
    let observer = TerminalObserver;
    let report = json_to_csv_with_observer(
        &input,
        &output,
        options.records_path.as_deref(),
        JsonToCsvOptions {
            delimiter,
            utf8_bom: options.utf8_bom,
            overwrite: options.overwrite,
            max_rows: options.max_rows,
        },
        &observer,
    )
    .map_err(error_text)?;
    print_report("json_to_csv", &report)
}

fn run_json_to_excel(options: CliOptions) -> Result<(), String> {
    let input = required_input(&options)?;
    let output = output_path(options.output.as_deref(), &input, "xlsx")?;
    let observer = TerminalObserver;
    let report = json_to_excel_with_observer(
        &input,
        &output,
        options.records_path.as_deref(),
        JsonToExcelOptions {
            overwrite: options.overwrite,
            max_rows: options.max_rows,
        },
        &observer,
    )
    .map_err(error_text)?;
    print_report("json_to_excel", &report)
}

fn run_csv_to_json(options: CliOptions) -> Result<(), String> {
    let input = required_input(&options)?;
    let output = output_path(options.output.as_deref(), &input, "json")?;
    let delimiter = parse_optional_delimiter(options.delimiter.as_deref()).map_err(error_text)?;
    let observer = TerminalObserver;
    let report = csv_to_json_with_observer(
        &input,
        &output,
        CsvToJsonOptions {
            delimiter,
            encoding: options.encoding,
            has_headers: options.has_headers,
            infer_types: options.infer_types,
            pretty: !options.compact,
            overwrite: options.overwrite,
            max_rows: options.max_rows,
        },
        &observer,
    )
    .map_err(error_text)?;
    print_report("csv_to_json", &report)
}

fn run_detect_csv(options: CliOptions) -> Result<(), String> {
    let input = required_input(&options)?;
    let delimiter = parse_optional_delimiter(options.delimiter.as_deref()).map_err(error_text)?;
    let detection = detect_csv(
        &input,
        options.encoding.as_deref(),
        delimiter,
        options.has_headers,
    )
    .map_err(error_text)?;
    let output = serde_json::json!({
        "ok": true,
        "operation": "detect_csv",
        "input_path": input,
        "csv_detection": detection,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&output).map_err(error_text)?
    );
    Ok(())
}

fn required_input(options: &CliOptions) -> Result<PathBuf, String> {
    resolve_path(
        options
            .input
            .as_deref()
            .ok_or_else(|| "an input file path is required".to_owned())?,
    )
}

fn input_kind(path: &Path) -> Result<InputKind, String> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => Ok(InputKind::Json),
        Some("csv" | "tsv") => Ok(InputKind::Csv),
        _ => Err(format!(
            "smart conversion supports .json, .csv, and .tsv files; got {}",
            path.display()
        )),
    }
}

fn output_path(requested: Option<&str>, input: &Path, extension: &str) -> Result<PathBuf, String> {
    match requested {
        Some(path) => resolve_path(path),
        None => Ok(input.with_extension(extension)),
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

fn print_report(operation: &str, report: &ConversionReport) -> Result<(), String> {
    let output = serde_json::json!({
        "ok": true,
        "operation": operation,
        "input_path": report.input_path,
        "output_path": report.output_path,
        "rows": report.rows,
        "columns": report.columns,
        "truncated": report.truncated,
        "csv_detection": report.csv_detection,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&output).map_err(error_text)?
    );
    Ok(())
}

struct TerminalObserver;

impl ConversionObserver for TerminalObserver {
    fn on_progress(&self, phase: ProgressPhase, rows: usize) {
        if (rows > 0 && rows % 10_000 == 0) || phase == ProgressPhase::Finalizing {
            eprintln!("{}: {rows} rows", phase_label(phase));
        }
    }
}

const fn phase_label(phase: ProgressPhase) -> &'static str {
    match phase {
        ProgressPhase::Detecting => "Detecting CSV format",
        ProgressPhase::Scanning => "Scanning JSON columns",
        ProgressPhase::Writing => "Writing output",
        ProgressPhase::Finalizing => "Finalizing output",
    }
}

fn error_text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub fn print_help() {
    println!(
        r#"json-to-csv-converter-mcp {version}

Start the MCP server with no arguments, or convert files directly from a terminal or Zed task.

USAGE:
  json-to-csv-converter-mcp
  json-to-csv-converter-mcp convert <FILE> [OPTIONS]
  json-to-csv-converter-mcp json-to-csv <FILE> [OPTIONS]
  json-to-csv-converter-mcp json-to-excel <FILE> [OPTIONS]
  json-to-csv-converter-mcp csv-to-json <FILE> [OPTIONS]
  json-to-csv-converter-mcp detect-csv <FILE> [OPTIONS]

SMART CONVERSION:
  .json files become .csv; .csv and .tsv files become .json.

OPTIONS:
  --output <PATH>             Set the output path
  --overwrite                 Replace an existing output file
  --max-rows <NUMBER>         Limit converted data rows
  --records-path <PATH>       Select nested JSON records
  --delimiter <VALUE>         auto, comma, semicolon, tab, pipe, or one ASCII character
  --encoding <LABEL>          Override CSV encoding detection
  --has-headers <MODE>        auto, true, or false
  --infer-types               Infer safe CSV value types in JSON output
  --compact                   Write compact JSON
  --utf8-bom                  Add a UTF-8 BOM to CSV output
  -h, --help                  Show this help
  -V, --version               Show the version

Existing output files are not overwritten unless --overwrite is supplied."#,
        version = env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_keyboard_friendly_cli_options() {
        let options = parse_options(
            &strings(&[
                "/tmp/people.csv",
                "--has-headers",
                "false",
                "--max-rows",
                "250",
                "--infer-types",
                "--overwrite",
            ]),
            CSV_TO_JSON_FLAGS,
        )
        .unwrap();

        assert_eq!(options.input.as_deref(), Some("/tmp/people.csv"));
        assert_eq!(options.has_headers, Some(false));
        assert_eq!(options.max_rows, Some(250));
        assert!(options.infer_types);
        assert!(options.overwrite);
    }

    #[test]
    fn rejects_flags_that_do_not_apply_to_a_command() {
        let error = parse_options(
            &strings(&["/tmp/people.json", "--encoding", "gbk"]),
            JSON_TO_CSV_FLAGS,
        )
        .unwrap_err();
        assert!(error.contains("not valid"));
    }

    #[test]
    fn recognizes_smart_conversion_extensions_case_insensitively() {
        assert_eq!(
            input_kind(Path::new("people.JSON")).unwrap(),
            InputKind::Json
        );
        assert_eq!(input_kind(Path::new("people.csv")).unwrap(), InputKind::Csv);
        assert_eq!(input_kind(Path::new("people.TSV")).unwrap(), InputKind::Csv);
        assert!(input_kind(Path::new("people.txt")).is_err());
    }

    #[test]
    fn requires_a_positive_max_rows_value() {
        let error = parse_options(
            &strings(&["/tmp/people.csv", "--max-rows", "0"]),
            CSV_TO_JSON_FLAGS,
        )
        .unwrap_err();
        assert!(error.contains("greater than zero"));
    }
}
