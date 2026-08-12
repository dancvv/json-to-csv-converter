use std::{
    collections::HashMap,
    fs::File,
    io::{Cursor, Read},
    path::Path,
};

use anyhow::{Context, Result, bail};
use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use csv::StringRecord;
use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
use encoding_rs_io::{DecodeReaderBytes, DecodeReaderBytesBuilder};
use serde::Serialize;

const DETECTION_SAMPLE_BYTES: u64 = 128 * 1024;
const DETECTION_RECORDS: usize = 100;
const DELIMITER_CANDIDATES: [u8; 4] = [b',', b';', b'\t', b'|'];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CsvDetection {
    pub encoding: String,
    pub encoding_source: String,
    pub delimiter: String,
    pub delimiter_source: String,
    pub has_headers: bool,
    pub header_source: String,
    pub sampled_records: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct DetectedCsv {
    pub report: CsvDetection,
    pub encoding: &'static Encoding,
    pub delimiter: u8,
}

pub(crate) type DecodedFile = DecodeReaderBytes<File, Vec<u8>>;

pub fn detect_csv(
    input_path: &Path,
    encoding_override: Option<&str>,
    delimiter_override: Option<u8>,
    has_headers_override: Option<bool>,
) -> Result<CsvDetection> {
    Ok(detect_csv_internal(
        input_path,
        encoding_override,
        delimiter_override,
        has_headers_override,
    )?
    .report)
}

pub(crate) fn detect_csv_internal(
    input_path: &Path,
    encoding_override: Option<&str>,
    delimiter_override: Option<u8>,
    has_headers_override: Option<bool>,
) -> Result<DetectedCsv> {
    let mut sample = Vec::new();
    File::open(input_path)
        .with_context(|| format!("failed to open CSV file {}", input_path.display()))?
        .take(DETECTION_SAMPLE_BYTES)
        .read_to_end(&mut sample)
        .with_context(|| format!("failed to sample CSV file {}", input_path.display()))?;

    if sample.is_empty() {
        bail!("CSV input is empty: {}", input_path.display());
    }
    if sample.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
        || sample.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
    {
        bail!("UTF-32 CSV files are not supported; save the file as UTF-8 or UTF-16 first");
    }

    let (encoding, encoding_source) = choose_encoding(&sample, encoding_override)?;
    let decoded_sample = decode_sample(&sample, encoding)?;

    let (delimiter, delimiter_source) = match delimiter_override {
        Some(delimiter) => (delimiter, "explicit".to_owned()),
        None => (detect_delimiter(&decoded_sample), "detected".to_owned()),
    };
    let records = sample_records(&decoded_sample, delimiter);
    if records.is_empty() {
        bail!("CSV input does not contain any readable records");
    }

    let (has_headers, header_source) = match has_headers_override {
        Some(has_headers) => (has_headers, "explicit".to_owned()),
        None => (detect_headers(&records), "detected".to_owned()),
    };

    Ok(DetectedCsv {
        report: CsvDetection {
            encoding: encoding.name().to_owned(),
            encoding_source,
            delimiter: delimiter_display(delimiter),
            delimiter_source,
            has_headers,
            header_source,
            sampled_records: records.len(),
        },
        encoding,
        delimiter,
    })
}

pub(crate) fn open_decoded_file(
    input_path: &Path,
    encoding: &'static Encoding,
) -> Result<DecodedFile> {
    let file = File::open(input_path)
        .with_context(|| format!("failed to open CSV file {}", input_path.display()))?;
    let mut builder = DecodeReaderBytesBuilder::new();
    builder
        .encoding(Some(encoding))
        .bom_override(true)
        .strip_bom(true)
        .utf8_passthru(true);
    Ok(builder.build(file))
}

fn choose_encoding(
    sample: &[u8],
    encoding_override: Option<&str>,
) -> Result<(&'static Encoding, String)> {
    if let Some(label) = encoding_override
        .map(str::trim)
        .filter(|label| !label.is_empty() && !label.eq_ignore_ascii_case("auto"))
    {
        let encoding = Encoding::for_label(label.as_bytes()).with_context(|| {
            format!(
                "unknown encoding `{label}`; use a standard label such as utf-8, utf-16le, gbk, big5, or windows-1252"
            )
        })?;
        return Ok((encoding, "explicit".to_owned()));
    }

    if sample.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok((UTF_8, "bom".to_owned()));
    }
    if sample.starts_with(&[0xFF, 0xFE]) {
        return Ok((UTF_16LE, "bom".to_owned()));
    }
    if sample.starts_with(&[0xFE, 0xFF]) {
        return Ok((UTF_16BE, "bom".to_owned()));
    }
    if std::str::from_utf8(sample).is_ok() {
        return Ok((UTF_8, "valid_utf8".to_owned()));
    }

    let mut detector = EncodingDetector::new(Iso2022JpDetection::Allow);
    detector.feed(sample, false);
    Ok((
        detector.guess(None, Utf8Detection::Allow),
        "statistical_detector".to_owned(),
    ))
}

fn decode_sample(sample: &[u8], encoding: &'static Encoding) -> Result<String> {
    let mut builder = DecodeReaderBytesBuilder::new();
    builder
        .encoding(Some(encoding))
        .bom_override(true)
        .strip_bom(true)
        .utf8_passthru(true);
    let mut reader = builder.build(Cursor::new(sample));
    let mut decoded = String::new();
    reader
        .read_to_string(&mut decoded)
        .context("failed to decode the CSV detection sample")?;
    Ok(decoded)
}

fn detect_delimiter(sample: &str) -> u8 {
    DELIMITER_CANDIDATES
        .into_iter()
        .map(|delimiter| {
            let records = sample_records(sample, delimiter);
            let mut widths = HashMap::<usize, usize>::new();
            for record in &records {
                if record.len() > 1 {
                    *widths.entry(record.len()).or_default() += 1;
                }
            }
            let (mode_width, consistent_records) = widths
                .into_iter()
                .max_by_key(|(width, count)| (*count, *width))
                .unwrap_or((1, 0));
            let score = consistent_records * 10_000 + mode_width * 100 + records.len();
            (delimiter, score)
        })
        .max_by_key(|(_, score)| *score)
        .filter(|(_, score)| *score > 0)
        .map(|(delimiter, _)| delimiter)
        .unwrap_or(b',')
}

fn sample_records(sample: &str, delimiter: u8) -> Vec<StringRecord> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(sample.as_bytes());
    let mut records = Vec::new();

    for result in reader.records().take(DETECTION_RECORDS) {
        match result {
            Ok(record) if !record.iter().all(|value| value.trim().is_empty()) => {
                records.push(record)
            }
            Ok(_) => {}
            // A fixed-size sample may end in the middle of a quoted record.
            Err(_) => break,
        }
    }
    records
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Empty,
    Boolean,
    Integer,
    Float,
    Text,
}

fn detect_headers(records: &[StringRecord]) -> bool {
    let Some(first) = records.first() else {
        return false;
    };
    if records.len() < 2 || first.is_empty() {
        return false;
    }
    if first.iter().any(|cell| cell.trim().is_empty()) {
        return false;
    }

    let mut score = 0i32;
    for column in 0..first.len() {
        let header = first.get(column).unwrap_or_default().trim();
        let data = records
            .iter()
            .skip(1)
            .filter_map(|record| record.get(column))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if data.is_empty() {
            continue;
        }

        let header_kind = cell_kind(header);
        let data_kind = majority_kind(&data);
        if header_kind == CellKind::Text && data_kind != CellKind::Text {
            score += 3;
        } else if header_kind != data_kind && data_kind != CellKind::Empty {
            score += 1;
        }

        if is_known_header(header) {
            score += 3;
        } else if looks_like_machine_header(header)
            && data.iter().any(|value| !looks_like_machine_header(value))
        {
            score += 1;
        }
    }

    score >= 2
}

fn majority_kind(values: &[&str]) -> CellKind {
    let mut counts = HashMap::<u8, usize>::new();
    for value in values {
        let kind = cell_kind(value);
        *counts.entry(kind as u8).or_default() += 1;
    }
    let kind = counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(kind, _)| kind)
        .unwrap_or(CellKind::Empty as u8);
    match kind {
        value if value == CellKind::Boolean as u8 => CellKind::Boolean,
        value if value == CellKind::Integer as u8 => CellKind::Integer,
        value if value == CellKind::Float as u8 => CellKind::Float,
        value if value == CellKind::Text as u8 => CellKind::Text,
        _ => CellKind::Empty,
    }
}

fn cell_kind(value: &str) -> CellKind {
    let value = value.trim();
    if value.is_empty() {
        CellKind::Empty
    } else if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
        CellKind::Boolean
    } else if value.parse::<i64>().is_ok() || value.parse::<u64>().is_ok() {
        CellKind::Integer
    } else if value.parse::<f64>().is_ok() {
        CellKind::Float
    } else {
        CellKind::Text
    }
}

fn looks_like_machine_header(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first == '_')
        && characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
}

fn is_known_header(value: &str) -> bool {
    const HEADERS: &[&str] = &[
        "id",
        "name",
        "first_name",
        "last_name",
        "email",
        "phone",
        "address",
        "city",
        "country",
        "date",
        "time",
        "datetime",
        "created_at",
        "updated_at",
        "status",
        "type",
        "category",
        "description",
        "note",
        "notes",
        "amount",
        "price",
        "quantity",
        "total",
        "code",
        "title",
        "url",
        "姓名",
        "名称",
        "编号",
        "序号",
        "邮箱",
        "电话",
        "手机",
        "地址",
        "城市",
        "国家",
        "日期",
        "时间",
        "状态",
        "类型",
        "分类",
        "备注",
        "金额",
        "价格",
        "数量",
        "部门",
        "标题",
    ];
    let normalized = value.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    HEADERS.contains(&normalized.as_str())
}

fn delimiter_display(delimiter: u8) -> String {
    match delimiter {
        b'\t' => "\\t".to_owned(),
        delimiter => char::from(delimiter).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use encoding_rs::GBK;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn detects_delimiters_and_headers() {
        for (name, content, delimiter) in [
            ("comma.csv", "id,name,age\n1,Alice,30\n2,Bob,31\n", ","),
            ("semicolon.csv", "id;name;age\n1;Alice;30\n2;Bob;31\n", ";"),
            (
                "tab.csv",
                "id\tname\tage\n1\tAlice\t30\n2\tBob\t31\n",
                "\\t",
            ),
            ("pipe.csv", "id|name|age\n1|Alice|30\n2|Bob|31\n", "|"),
        ] {
            let directory = tempdir().unwrap();
            let path = directory.path().join(name);
            fs::write(&path, content).unwrap();
            let detection = detect_csv(&path, None, None, None).unwrap();
            assert_eq!(detection.delimiter, delimiter);
            assert!(detection.has_headers);
            assert_eq!(detection.encoding, "UTF-8");
        }
    }

    #[test]
    fn detects_headerless_numeric_data() {
        let records = sample_records("1,2,3\n4,5,6\n", b',');
        assert!(!detect_headers(&records));
    }

    #[test]
    fn detects_gbk_chinese_csv() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("people-gbk.csv");
        let (bytes, _, _) = GBK.encode("姓名,城市\n张三,上海\n李四,北京\n");
        fs::write(&path, bytes).unwrap();

        let detection = detect_csv(&path, None, None, None).unwrap();
        assert_eq!(detection.encoding, "GBK");
        assert_eq!(detection.delimiter, ",");
        assert!(detection.has_headers);
    }
}
