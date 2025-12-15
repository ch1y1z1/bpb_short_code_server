# bpb_short_code_server

一个非常简单的短码服务（无前端），提供 2 个 API：

- `POST /encode`：上传原始字符串，返回 **2–5 位**短字符串（去重）。
- `POST /decode`：上传短字符串，返回当时上传的原始字符串。

## 运行

```bash
cargo run
```

默认：

- **监听地址**：`0.0.0.0:3000`
- **SQLite 文件**：`./shortcodes.db`
  - 若文件/父目录不存在，会在启动时自动创建。

可选环境变量：

- **`LISTEN_ADDR`**：例如 `0.0.0.0:3000`
- **`DATABASE_URL`**：
  - 文件：`sqlite://./shortcodes.db`（默认）
  - 绝对路径：`sqlite:///tmp/shortcodes.db`
  - 内存：`sqlite::memory:`

## API

### `POST /encode`

**用途**：上传原始字符串 `value`，返回短码 `code`。同一个 `value` 多次提交，会返回同一个 `code`（去重）。

**Request JSON**

```json
{
  "value": "hello world"
}
```

**Response JSON**

```json
{
  "code": "01"
}
```

**curl 示例**

```bash
curl -sS -X POST 'http://127.0.0.1:3000/encode' \
  -H 'content-type: application/json' \
  -d '{"value":"hello world"}'
```

**错误**

- `400`：`value` 为空
- `507`：短码空间耗尽（当前实现限制短码最长 5 位 base62；你的数据量不大通常不会触发）

### `POST /decode`

**用途**：上传短码 `code`，返回原始字符串 `value`。

**Request JSON**

```json
{
  "code": "01"
}
```

**Response JSON**

```json
{
  "value": "hello world"
}
```

**curl 示例**

```bash
curl -sS -X POST 'http://127.0.0.1:3000/decode' \
  -H 'content-type: application/json' \
  -d '{"code":"01"}'
```

**错误**

- `400`：`code` 长度不是 2..=5，或包含非法字符（仅允许 `0-9a-zA-Z`）
- `404`：找不到该 `code`

## 说明（实现细节）

- 短码生成：使用 SQLite 自增 `id` 做 base62 编码，天然唯一；不足 2 位会在左侧补 `0`。
- 存储：SQLite 表 `mappings`，其中 `value` 和 `code` 都是 `UNIQUE`，保证去重与反查。
