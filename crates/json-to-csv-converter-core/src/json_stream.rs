use std::{
    fmt,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{Context, Result, bail};
use serde::de::Error as _;
use serde::{
    Deserialize,
    de::{
        self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor,
        value::MapAccessDeserializer,
    },
};
use serde_json::Value;

use crate::{ConversionObserver, ProgressPhase};

const CANCELLED_MARKER: &str = "__JSON_TO_CSV_CONVERTER_CANCELLED__";
const LIMIT_MARKER: &str = "__JSON_TO_CSV_CONVERTER_ROW_LIMIT__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamOutcome {
    pub rows: usize,
    pub truncated: bool,
}

pub(crate) fn stream_json_records<F>(
    input_path: &Path,
    records_path: Option<&str>,
    max_rows: Option<usize>,
    observer: &dyn ConversionObserver,
    phase: ProgressPhase,
    mut consume: F,
) -> Result<StreamOutcome>
where
    F: FnMut(Value) -> Result<()>,
{
    if max_rows == Some(0) {
        bail!("max_rows must be greater than zero");
    }

    let mut file = File::open(input_path)
        .with_context(|| format!("failed to open JSON file {}", input_path.display()))?;
    skip_utf8_bom(&mut file)?;
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
    let segments = parse_records_path(records_path)?;
    let mut state = StreamState {
        consume: &mut consume,
        observer,
        phase,
        max_rows,
        rows: 0,
        truncated: false,
    };
    let mut found = segments.is_empty();

    let result = if segments.is_empty() {
        RecordCollectionSeed { state: &mut state }.deserialize(&mut deserializer)
    } else {
        PathSeed {
            segments: &segments,
            state: &mut state,
            found: &mut found,
        }
        .deserialize(&mut deserializer)
    };

    match result {
        Ok(()) => deserializer
            .end()
            .with_context(|| format!("invalid JSON in {}", input_path.display()))?,
        Err(error) if error.to_string().contains(LIMIT_MARKER) => {
            state.truncated = true;
        }
        Err(error) if error.to_string().contains(CANCELLED_MARKER) => {
            bail!("conversion cancelled");
        }
        Err(error) => {
            return Err(error).with_context(|| format!("invalid JSON in {}", input_path.display()));
        }
    }

    if !found {
        let path = records_path.unwrap_or_default();
        bail!("records_path `{path}` was not found");
    }

    Ok(StreamOutcome {
        rows: state.rows,
        truncated: state.truncated,
    })
}

fn skip_utf8_bom(file: &mut File) -> Result<()> {
    let mut prefix = [0_u8; 3];
    let read = file
        .read(&mut prefix)
        .context("failed to inspect JSON BOM")?;
    if read != prefix.len() || prefix != [0xEF, 0xBB, 0xBF] {
        file.seek(SeekFrom::Start(0))
            .context("failed to rewind JSON input")?;
    }
    Ok(())
}

fn parse_records_path(records_path: Option<&str>) -> Result<Vec<String>> {
    let Some(path) = records_path.map(str::trim).filter(|path| !path.is_empty()) else {
        return Ok(Vec::new());
    };

    if path.starts_with('/') {
        return path
            .split('/')
            .skip(1)
            .map(|segment| decode_json_pointer_segment(segment, path))
            .collect();
    }

    path.split('.')
        .map(|segment| {
            if segment.is_empty() {
                bail!("records_path `{path}` contains an empty segment");
            }
            Ok(segment.to_owned())
        })
        .collect()
}

fn decode_json_pointer_segment(segment: &str, path: &str) -> Result<String> {
    let mut decoded = String::with_capacity(segment.len());
    let mut characters = segment.chars();
    while let Some(character) = characters.next() {
        if character != '~' {
            decoded.push(character);
            continue;
        }
        match characters.next() {
            Some('0') => decoded.push('~'),
            Some('1') => decoded.push('/'),
            _ => bail!("records_path `{path}` contains an invalid JSON Pointer escape"),
        }
    }
    Ok(decoded)
}

struct StreamState<'a, F> {
    consume: &'a mut F,
    observer: &'a dyn ConversionObserver,
    phase: ProgressPhase,
    max_rows: Option<usize>,
    rows: usize,
    truncated: bool,
}

impl<F> StreamState<'_, F>
where
    F: FnMut(Value) -> Result<()>,
{
    fn consume<E>(&mut self, value: Value) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        if self.observer.is_cancelled() {
            return Err(E::custom(CANCELLED_MARKER));
        }
        if self.max_rows.is_some_and(|limit| self.rows >= limit) {
            self.truncated = true;
            return Err(E::custom(LIMIT_MARKER));
        }

        (self.consume)(value).map_err(|error| E::custom(error.to_string()))?;
        self.rows += 1;
        self.observer.on_progress(self.phase, self.rows);
        Ok(())
    }
}

struct RecordCollectionSeed<'a, 'b, F> {
    state: &'a mut StreamState<'b, F>,
}

impl<'de, F> DeserializeSeed<'de> for RecordCollectionSeed<'_, '_, F>
where
    F: FnMut(Value) -> Result<()>,
{
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(RecordCollectionVisitor { state: self.state })
    }
}

struct RecordCollectionVisitor<'a, 'b, F> {
    state: &'a mut StreamState<'b, F>,
}

impl<'de, F> Visitor<'de> for RecordCollectionVisitor<'_, '_, F>
where
    F: FnMut(Value) -> Result<()>,
{
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON array or a single JSON value")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(value) = sequence.next_element::<Value>()? {
            self.state.consume(value)?;
        }
        Ok(())
    }

    fn visit_map<A>(self, map: A) -> std::result::Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        let value = Value::deserialize(MapAccessDeserializer::new(map))?;
        self.state.consume(value)
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        self.state.consume(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        self.state.consume(Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        self.state.consume(Value::from(value))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        self.state.consume(Value::from(value))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        self.state.consume(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        self.state.consume(Value::String(value))
    }

    fn visit_none<E>(self) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        self.state.consume(Value::Null)
    }

    fn visit_unit<E>(self) -> std::result::Result<(), E>
    where
        E: de::Error,
    {
        self.state.consume(Value::Null)
    }
}

struct PathSeed<'a, 'b, 'c, F> {
    segments: &'a [String],
    state: &'b mut StreamState<'c, F>,
    found: &'b mut bool,
}

impl<'de, F> DeserializeSeed<'de> for PathSeed<'_, '_, '_, F>
where
    F: FnMut(Value) -> Result<()>,
{
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if self.segments.is_empty() {
            *self.found = true;
            return RecordCollectionSeed { state: self.state }.deserialize(deserializer);
        }
        deserializer.deserialize_any(PathVisitor {
            segments: self.segments,
            state: self.state,
            found: self.found,
        })
    }
}

struct PathVisitor<'a, 'b, 'c, F> {
    segments: &'a [String],
    state: &'b mut StreamState<'c, F>,
    found: &'b mut bool,
}

impl<'de, F> Visitor<'de> for PathVisitor<'_, '_, '_, F>
where
    F: FnMut(Value) -> Result<()>,
{
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object or array along records_path")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        let target = &self.segments[0];
        let state = self.state;
        let found = self.found;
        while let Some(key) = map.next_key::<String>()? {
            if key == *target && !*found {
                map.next_value_seed(PathSeed {
                    segments: &self.segments[1..],
                    state,
                    found,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        let target = self.segments[0]
            .parse::<usize>()
            .map_err(|_| A::Error::custom("array records_path segment must be an index"))?;
        let state = self.state;
        let found = self.found;
        let mut index = 0;
        loop {
            if index == target && !*found {
                let value = sequence.next_element_seed(PathSeed {
                    segments: &self.segments[1..],
                    state,
                    found,
                })?;
                if value.is_none() {
                    break;
                }
            } else if sequence.next_element::<IgnoredAny>()?.is_none() {
                break;
            }
            index += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Mutex};

    use tempfile::tempdir;

    use super::*;

    #[derive(Default)]
    struct Observer(Mutex<Vec<usize>>);

    impl ConversionObserver for Observer {
        fn on_progress(&self, _phase: ProgressPhase, rows: usize) {
            self.0.lock().unwrap().push(rows);
        }
    }

    #[test]
    fn streams_nested_records_and_honors_limit() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("wrapped.json");
        fs::write(
            &input,
            r#"{"payload":{"items":[{"id":1},{"id":2},{"id":3}]},"other":[1,2,3]}"#,
        )
        .unwrap();
        let observer = Observer::default();
        let mut ids = Vec::new();

        let outcome = stream_json_records(
            &input,
            Some("payload.items"),
            Some(2),
            &observer,
            ProgressPhase::Writing,
            |value| {
                ids.push(value["id"].as_u64().unwrap());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(ids, [1, 2]);
        assert_eq!(outcome.rows, 2);
        assert!(outcome.truncated);
        assert_eq!(*observer.0.lock().unwrap(), [1, 2]);
    }
}
