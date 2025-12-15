use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, sqlite::SqlitePoolOptions};
use std::path::Path;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    pool: sqlx::SqlitePool,
}

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    #[error("short code space exhausted (max 5 base62 chars)")]
    Exhausted,
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::Exhausted => (StatusCode::INSUFFICIENT_STORAGE, self.to_string()),
            ApiError::Sqlx(e) => {
                error!(error = %e, "database error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(ErrorResponse { error: msg })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

#[derive(Deserialize)]
struct EncodeRequest {
    value: String,
}

#[derive(Serialize)]
struct EncodeResponse {
    code: String,
}

#[derive(Deserialize)]
struct DecodeRequest {
    code: String,
}

#[derive(Serialize)]
struct DecodeResponse {
    value: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let db_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://./shortcodes.db".to_string());
    let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());

    info!(%db_url, %listen_addr, "starting");

    ensure_sqlite_file_exists(&db_url)?;

    let pool = SqlitePoolOptions::new()
        .max_connections(10)
        .connect(&db_url)
        .await?;

    init_db(&pool).await?;

    let app = Router::new()
        .route("/encode", post(encode))
        .route("/decode", post(decode))
        .with_state(AppState { pool });

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn ensure_sqlite_file_exists(db_url: &str) -> anyhow::Result<()> {
    // sqlx sqlite 会在需要时创建文件，但这里额外做一层保证：
    // - 若 DB 文件路径的父目录不存在，先创建目录
    // - 若 DB 文件不存在，先 touch 创建文件
    let Some(mut path) = sqlite_file_path_from_url(db_url) else {
        return Ok(());
    };

    // 去掉可能的 querystring（例如 ?mode=rwc）
    if let Some((p, _q)) = path.split_once('?') {
        path = p;
    }

    if path.is_empty() {
        return Ok(());
    }

    // 内存库：sqlite::memory: / :memory:
    if path == ":memory:" || path == "file::memory:" {
        return Ok(());
    }

    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    if !p.exists() {
        let _f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(p)?;
    }

    Ok(())
}

fn sqlite_file_path_from_url(db_url: &str) -> Option<&str> {
    if db_url == "sqlite::memory:" {
        return None;
    }

    if let Some(rest) = db_url.strip_prefix("sqlite://") {
        return Some(rest);
    }

    if let Some(rest) = db_url.strip_prefix("sqlite:") {
        // 兼容 sqlite:./db.sqlite 或 sqlite:///abs/path.sqlite
        return Some(rest.strip_prefix("//").unwrap_or(rest));
    }

    None
}

async fn init_db(pool: &sqlx::SqlitePool) -> Result<(), sqlx::Error> {
    // value: 原始字符串（去重）
    // code: 2-5 位短字符串（唯一）
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS mappings (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            code        TEXT UNIQUE,
            value       TEXT NOT NULL UNIQUE,
            created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        );
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(r#"CREATE INDEX IF NOT EXISTS idx_mappings_code ON mappings(code);"#)
        .execute(pool)
        .await?;

    Ok(())
}

async fn encode(State(state): State<AppState>, Json(req): Json<EncodeRequest>) -> ApiResult<EncodeResponse> {
    if req.value.is_empty() {
        return Err(ApiError::BadRequest("value is empty".to_string()));
    }

    // 快路径：已存在则直接返回
    if let Some(code) = sqlx::query_scalar::<_, String>("SELECT code FROM mappings WHERE value = ?1")
        .bind(&req.value)
        .fetch_optional(&state.pool)
        .await?
    {
        return Ok(Json(EncodeResponse { code }));
    }

    let mut tx = state.pool.begin().await?;

    // 并发安全：同一个 value 只插入一次
    sqlx::query("INSERT INTO mappings (value) VALUES (?1) ON CONFLICT(value) DO NOTHING")
        .bind(&req.value)
        .execute(&mut *tx)
        .await?;

    let row = sqlx::query("SELECT id, code FROM mappings WHERE value = ?1")
        .bind(&req.value)
        .fetch_one(&mut *tx)
        .await?;

    let id: i64 = row.get("id");
    let code: Option<String> = row.get("code");

    let final_code = if let Some(code) = code {
        code
    } else {
        let new_code = id_to_code(id)?;
        sqlx::query("UPDATE mappings SET code = ?1 WHERE id = ?2 AND code IS NULL")
            .bind(&new_code)
            .bind(id)
            .execute(&mut *tx)
            .await?;

        let code = sqlx::query_scalar::<_, String>("SELECT code FROM mappings WHERE id = ?1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        code
    };

    tx.commit().await?;
    Ok(Json(EncodeResponse { code: final_code }))
}

async fn decode(State(state): State<AppState>, Json(req): Json<DecodeRequest>) -> ApiResult<DecodeResponse> {
    validate_code(&req.code)?;

    let value = sqlx::query_scalar::<_, String>("SELECT value FROM mappings WHERE code = ?1")
        .bind(&req.code)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(DecodeResponse { value }))
}

fn validate_code(code: &str) -> Result<(), ApiError> {
    let len = code.len();
    if !(2..=5).contains(&len) {
        return Err(ApiError::BadRequest("code length must be 2..=5".to_string()));
    }
    if !code
        .as_bytes()
        .iter()
        .all(|&b| CHARSET.contains(&b))
    {
        return Err(ApiError::BadRequest(
            "code contains invalid characters".to_string(),
        ));
    }
    Ok(())
}

const CHARSET: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn id_to_code(id: i64) -> Result<String, ApiError> {
    if id <= 0 {
        return Err(ApiError::BadRequest("invalid id".to_string()));
    }
    let mut n = id as u64;

    let mut buf = Vec::new();
    while n > 0 {
        let rem = (n % 62) as usize;
        buf.push(CHARSET[rem]);
        n /= 62;
    }
    buf.reverse();
    let mut s = String::from_utf8(buf).expect("charset is ascii");

    if s.len() > 5 {
        return Err(ApiError::Exhausted);
    }
    if s.len() < 2 {
        s = format!("{:0>2}", s);
    }
    Ok(s)
}
