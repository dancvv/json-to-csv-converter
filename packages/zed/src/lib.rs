use std::path::Path;

use zed::process;
use zed::settings::ContextServerSettings;
use zed::{serde_json::Value, ContextServerConfiguration, ContextServerId, Os};
use zed_extension_api as zed;

const CONTEXT_SERVER_ID: &str = "json-to-csv-converter";
const DEFAULT_BINARY: &str = "json-to-csv-converter-mcp";
const BASE_DIRECTORY_ENV: &str = "JSON_TO_CSV_CONVERTER_BASE_DIR";

struct JsonToCsvConverterExtension;

impl zed::Extension for JsonToCsvConverterExtension {
    fn new() -> Self {
        Self
    }

    fn context_server_command(
        &mut self,
        context_server_id: &ContextServerId,
        project: &zed::Project,
    ) -> zed::Result<zed::Command> {
        if context_server_id.as_ref() != CONTEXT_SERVER_ID {
            return Err(format!("unknown context server `{context_server_id}`"));
        }

        let zed_settings = ContextServerSettings::for_project(CONTEXT_SERVER_ID, project)?;
        let converter_settings = ConverterSettings::from_value(zed_settings.settings.as_ref())?;
        let command_settings = zed_settings.command;

        let requested_command = command_settings
            .as_ref()
            .and_then(|command| command.path.clone())
            .or(converter_settings.binary_path)
            .unwrap_or_else(|| DEFAULT_BINARY.to_owned());
        let command = resolve_binary(&requested_command)?;
        validate_binary(&command)?;

        let args = command_settings
            .as_ref()
            .and_then(|command| command.arguments.clone())
            .unwrap_or_default();
        let mut env = command_settings
            .and_then(|command| command.env)
            .unwrap_or_default();

        if let Some(base_directory) = converter_settings.base_directory {
            env.insert(BASE_DIRECTORY_ENV.to_owned(), base_directory);
        }

        let mut env = env.into_iter().collect::<Vec<_>>();
        env.sort_by(|left, right| left.0.cmp(&right.0));

        Ok(zed::Command { command, args, env })
    }

    fn context_server_configuration(
        &mut self,
        context_server_id: &ContextServerId,
        _project: &zed::Project,
    ) -> zed::Result<Option<ContextServerConfiguration>> {
        if context_server_id.as_ref() != CONTEXT_SERVER_ID {
            return Ok(None);
        }

        Ok(Some(ContextServerConfiguration {
            installation_instructions: INSTALLATION_INSTRUCTIONS.to_owned(),
            settings_schema: SETTINGS_SCHEMA.to_owned(),
            default_settings: DEFAULT_SETTINGS.to_owned(),
        }))
    }
}

#[derive(Default)]
struct ConverterSettings {
    binary_path: Option<String>,
    base_directory: Option<String>,
}

impl ConverterSettings {
    fn from_value(value: Option<&Value>) -> zed::Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let Some(settings) = value.as_object() else {
            return Err(
                "`context_servers.json-to-csv-converter.settings` must be an object".to_owned(),
            );
        };

        Ok(Self {
            binary_path: optional_string(settings.get("binary_path"), "binary_path")?,
            base_directory: optional_string(settings.get("base_directory"), "base_directory")?,
        })
    }
}

fn optional_string(value: Option<&Value>, key: &str) -> zed::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| format!("`{key}` must be a string or null"))
}

fn validate_binary(command: &str) -> zed::Result<()> {
    let output = process::Command::new(command)
        .arg("--version")
        .output()
        .map_err(|error| {
            format!(
                "Could not run `{command} --version`. Install json-to-csv-converter-mcp first, or set `context_servers.json-to-csv-converter.settings.binary_path` to its absolute path. Underlying error: {error}"
            )
        })?;

    if output.status == Some(0) {
        return Ok(());
    }

    Err(format!(
        "`{command} --version` exited with status {:?}.\n\nstdout:\n{}\n\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn resolve_binary(command: &str) -> zed::Result<String> {
    if Path::new(command).is_absolute() {
        return Ok(command.to_owned());
    }
    if command != DEFAULT_BINARY {
        return Err(format!(
            "`binary_path` must be absolute when it is not `{DEFAULT_BINARY}`; received `{command}`"
        ));
    }

    let locator = match zed::current_platform().0 {
        Os::Windows => "where",
        Os::Mac | Os::Linux => "which",
    };
    let output = process::Command::new(locator)
        .arg(DEFAULT_BINARY)
        .output()
        .map_err(|error| format!("Could not locate `{DEFAULT_BINARY}` with `{locator}`: {error}"))?;
    if output.status != Some(0) {
        return Err(format!(
            "Could not find `{DEFAULT_BINARY}` on PATH. Install it first or configure an absolute `binary_path`."
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| format!("`{locator}` returned no path for `{DEFAULT_BINARY}`"))?;
    if !Path::new(path).is_absolute() {
        return Err(format!(
            "`{locator}` returned a non-absolute path for `{DEFAULT_BINARY}`: {path}"
        ));
    }
    Ok(path.to_owned())
}

const INSTALLATION_INSTRUCTIONS: &str = r#"Install the companion MCP server, then restart or reload Zed:

```sh
cargo install --git https://github.com/dancvv/json-to-csv-converter --package json-to-csv-converter-mcp
```

If Zed cannot find `json-to-csv-converter-mcp` on PATH, set an absolute `binary_path` in this extension's MCP settings. You can also set `base_directory` to resolve relative tool paths against a specific folder."#;

const DEFAULT_SETTINGS: &str = r#"{
  "settings": {
    "binary_path": "json-to-csv-converter-mcp",
    "base_directory": null
  }
}"#;

const SETTINGS_SCHEMA: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "properties": {
    "settings": {
      "type": "object",
      "properties": {
        "binary_path": {
          "type": "string",
          "description": "Absolute path or PATH-resolved name of the json-to-csv-converter-mcp executable."
        },
        "base_directory": {
          "type": ["string", "null"],
          "description": "Optional base directory used to resolve relative input and output paths."
        }
      },
      "additionalProperties": false
    }
  }
}"#;

zed::register_extension!(JsonToCsvConverterExtension);
