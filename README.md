# JSON to CSV Converter for Zed

一个通过 Zed Agent Panel 使用的 JSON、CSV、Excel 转换插件。支持 CSV 格式自动识别、大文件流式读写、转换进度、最大行数限制和中途取消，提供四个 MCP 工具：

- `json_to_csv`：JSON → CSV
- `json_to_excel`：JSON → Excel `.xlsx`
- `csv_to_json`：CSV → JSON
- `detect_csv_format`：只检测 CSV 的编码、分隔符和表头，不生成文件

> Zed 当前没有面向普通扩展开放“读取当前编辑器并另存文件”的通用 API，并且已经移除了扩展自定义 Slash Command，因此本项目按官方推荐实现为 MCP Server Extension。转换服务本身与 Zed 解耦，后续也可以直接发布到 MCP Registry。

## 功能特点

- 支持 JSON 对象数组、单个对象和基本类型数组。
- 嵌套对象自动展开为点号列名，例如 `address.city`。
- 数组值保留为 JSON 字符串，不会静默丢失数据。
- 不同 JSON 记录可以拥有不同字段，表头按首次出现顺序合并。
- 包装型 JSON 可通过 `records_path` 选择记录，例如 `payload.items` 或 `/payload/items`。
- CSV 正确处理引号、逗号、换行、中文和 UTF-8 BOM。
- CSV 自动识别逗号、分号、Tab、管道、字符编码和是否有表头，也允许手动覆盖检测结果。
- 编码检测支持 BOM、UTF-8，以及 GBK、Big5、Shift_JIS、Windows-1252 等常见传统编码。
- CSV → JSON 默认保留全部字符串，避免把 `001` 误转成数字；可选类型推断。
- Excel 输出包含类型化单元格、样式表头、冻结首行、筛选器和自适应列宽。
- 转换过程采用有界内存的流式处理：CSV 单遍写出；JSON 先扫描列名、再流式写出；Excel 使用 constant-memory 模式。
- 转换工具支持 MCP 进度通知、客户端取消和 `max_rows` 最大数据行数限制。
- 默认拒绝覆盖已有文件，写入 CSV/JSON/XLSX 时使用同目录临时文件提交。
- 转换失败或取消时自动清理临时文件，不会留下不完整的目标文件。

## 工程结构

```text
json-to-csv-converter/
├── crates/
│   ├── json-to-csv-converter-core/   # JSON、CSV、XLSX 转换核心
│   └── json-to-csv-converter-mcp/    # stdio MCP 服务
├── packages/zed/              # Zed Wasm 扩展启动层
└── examples/                  # 示例数据
```

## 安装

要求 Rust 1.88 或更高版本。

1. 安装 MCP 服务：

   ```sh
   cargo install --git https://github.com/dancvv/json-to-csv-converter --package json-to-csv-converter-mcp
   ```

2. 安装 Zed 的 Wasm 编译目标：

   ```sh
   rustup target add wasm32-wasip2
   ```

3. 在 Zed 扩展市场安装 `JSON to CSV Converter`。市场审核期间，也可以克隆本仓库，在 Zed 命令面板执行 `zed: install dev extension` 并选择：

   ```text
   /path/to/json-to-csv-converter/packages/zed
   ```

4. 在 Agent Panel 的 MCP 设置中启用 `JSON to CSV Converter`。

如果从桌面启动的 Zed 找不到 Cargo 安装目录，请给 `binary_path` 配置绝对路径。可选的 `base_directory` 用来解析工具参数中的相对路径：

```json
{
  "context_servers": {
    "json-to-csv-converter": {
      "settings": {
        "binary_path": "/Users/you/.cargo/bin/json-to-csv-converter-mcp",
        "base_directory": "/Users/you/projects/demo"
      }
    }
  }
}
```

## 在 Zed 中使用

可以直接在 Agent Panel 中这样说：

```text
把 /Users/you/data/people.json 转换成 CSV
把 /Users/you/data/people.json 转换成 Excel 文件
把 /Users/you/data/people.csv 转换成 JSON，保留所有字段为字符串
检查 /Users/you/data/import.csv 的编码、分隔符和表头
把 /Users/you/data/large.csv 转换成 JSON，最多处理 100000 行
把 payload.json 里的 payload.items 转换成 Excel
```

默认输出到输入文件旁边，并替换扩展名。目标文件存在时转换会停止；只有明确要求覆盖时，Agent 才应传入 `overwrite: true`。

长时间转换时，支持进度通知的 MCP 客户端会显示当前阶段和已处理行数。客户端发送取消请求后，转换会尽快停止；正在处理的单条超大记录需要先解析完成才会响应取消。

### 工具参数

| 工具 | 主要可选参数 | 默认值 |
|---|---|---|
| `json_to_csv` | `output_path`, `records_path`, `delimiter`, `utf8_bom`, `max_rows`, `overwrite` | 逗号、无 BOM、全部行、不覆盖 |
| `json_to_excel` | `output_path`, `records_path`, `max_rows`, `overwrite` | 全部行、不覆盖 |
| `csv_to_json` | `output_path`, `delimiter`, `encoding`, `has_headers`, `infer_types`, `pretty`, `max_rows`, `overwrite` | 自动检测格式、字符串、格式化、全部行、不覆盖 |
| `detect_csv_format` | `encoding`, `delimiter`, `has_headers` | 全部自动检测 |

`delimiter` 接受 `auto`、`comma`、`semicolon`、`tab`、`pipe`，也接受一个 ASCII 字符。`encoding` 接受标准编码标签，例如 `utf-8`、`utf-16le`、`gbk`、`big5`、`shift_jis` 和 `windows-1252`。`has_headers` 可传 `true` 或 `false` 强制指定。

自动检测只读取文件开头最多 128 KiB、最多 100 条记录，因此不会因为检测大文件而把整个文件读入内存。表头识别属于启发式判断；遇到结构特殊的无表头文件时，建议明确传入 `has_headers: false`。

设置 `max_rows` 后，结果中的 `truncated` 会说明源文件是否还有未写出的数据。`csv_to_json` 的结果还会返回 `csv_detection`，包含最终采用的编码、分隔符、表头判断及其来源（自动检测或显式参数）。

## 数据转换规则

下面的 JSON：

```json
[
  {
    "id": "001",
    "name": "张三",
    "address": { "city": "上海" },
    "tags": ["研发", "AI"]
  }
]
```

转换后的表头和值为：

```csv
id,name,address.city,tags
001,张三,上海,"[""研发"",""AI""]"
```

CSV → JSON 默认不猜测类型。启用 `infer_types` 后，只推断小写 `true`、`false`、`null` 以及无歧义数字；`001` 仍保持字符串。

## 大文件处理说明

- JSON → CSV / Excel 会读取两遍输入：第一遍收集所有可能的列名，第二遍逐行写出，因此内存不会随记录总数线性增长。
- CSV → JSON 在最多 128 KiB 的格式检测之后逐条解码、解析并写出。
- 单条 JSON 记录仍需完整放入内存；如果某一个对象本身非常大，它仍会决定峰值内存。
- JSON 两遍读取期间请不要修改源文件；如果第二遍出现新列，转换会停止，避免静默丢字段。
- Excel 本身最多支持 1,048,575 条数据行（另加一行表头）和 16,384 列。

## 开发与验证

```sh
# 转换核心和 MCP 服务测试
cargo test --workspace

# 构建 MCP 服务
cargo build -p json-to-csv-converter-mcp

# 检查 Zed 扩展
cargo check --manifest-path packages/zed/Cargo.toml --target wasm32-wasip2
```

## 开源信息

- GitHub：<https://github.com/dancvv/json-to-csv-converter>
- 作者：spicytree (<uquantum@hotmail.com>)
- 许可证：MIT
