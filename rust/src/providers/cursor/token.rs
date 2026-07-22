//! Cursor credential reader.
//!
//! Cursor's desktop app stores its auth tokens in a VS Code-style SQLite
//! state database. On Windows this defaults to
//! `%APPDATA%\Cursor\User\globalStorage\state.vscdb`. The access token is a
//! JWT that Cursor sends as `Authorization: Bearer` to `api2.cursor.sh`, so
//! reading it lets us fetch usage without scraping browser cookies.

use rusqlite::{Connection, OpenFlags, types::Value as SqlValue};
use std::path::{Path, PathBuf};

use crate::core::ProviderError;

const ACCESS_TOKEN_KEY: &str = "cursorAuth/accessToken";
const EMAIL_KEY: &str = "cursorAuth/cachedEmail";

/// Credentials read from Cursor's local state database.
#[derive(Clone, Debug)]
pub(super) struct CursorCredentials {
    pub access_token: String,
    pub email: Option<String>,
}

/// Resolve the Cursor `state.vscdb` path, honoring a `CURSOR_STATE_DB` override.
pub(super) fn default_db_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CURSOR_STATE_DB")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path));
    }

    // Electron's userData directory maps to `dirs::config_dir()` on every
    // platform: Windows `%APPDATA%`, macOS `~/Library/Application Support`,
    // Linux `~/.config`.
    dirs::config_dir().map(|base| {
        base.join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    })
}

/// Read Cursor credentials from the default (or overridden) state database.
pub(super) fn read_credentials() -> Result<CursorCredentials, ProviderError> {
    let path = default_db_path().ok_or_else(|| {
        ProviderError::NotInstalled("Could not resolve Cursor application data path.".to_string())
    })?;
    read_credentials_from(&path)
}

fn read_credentials_from(db_path: &Path) -> Result<CursorCredentials, ProviderError> {
    if !db_path.exists() {
        return Err(ProviderError::NotInstalled(format!(
            "Cursor database not found at {}. Open Cursor and sign in first.",
            db_path.display()
        )));
    }

    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| ProviderError::Other(format!("Failed to open Cursor database: {e}")))?;
    conn.busy_timeout(std::time::Duration::from_millis(250))
        .map_err(|e| ProviderError::Other(format!("Failed to configure SQLite timeout: {e}")))?;

    let access_token = read_string_value(&conn, ACCESS_TOKEN_KEY)
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => ProviderError::AuthRequired,
            other => ProviderError::Other(format!("Failed to read Cursor access token: {other}")),
        })?
        .filter(|token| !token.is_empty())
        .ok_or(ProviderError::AuthRequired)?;

    // Email is a nicety for the account label; never fail the fetch over it.
    let email = read_string_value(&conn, EMAIL_KEY)
        .ok()
        .flatten()
        .filter(|value| !value.is_empty());

    Ok(CursorCredentials {
        access_token,
        email,
    })
}

fn read_string_value(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    let value: SqlValue = conn.query_row(
        "SELECT value FROM ItemTable WHERE key = ?1 LIMIT 1",
        [key],
        |row| row.get(0),
    )?;
    Ok(decode_string_value(value))
}

/// VS Code stores `ItemTable` values as UTF-8 text (occasionally UTF-16 blobs).
/// Cursor keeps the token as a bare string, but tolerate JSON-quoted forms too.
fn decode_string_value(value: SqlValue) -> Option<String> {
    let raw = match value {
        SqlValue::Text(text) => text,
        SqlValue::Blob(bytes) => decode_blob(&bytes)?,
        _ => return None,
    };
    Some(unquote(raw.trim_matches(char::from(0)).trim()).to_string())
}

fn decode_blob(bytes: &[u8]) -> Option<String> {
    // Interior NULs mean this is really UTF-16-LE (each ASCII char is followed
    // by a 0x00 that is itself valid UTF-8), so only accept a NUL-free decode.
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.contains('\0')
    {
        return Some(text.to_string());
    }

    if bytes.len().is_multiple_of(2) {
        let utf16: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        if let Ok(text) = String::from_utf16(&utf16) {
            return Some(text);
        }
    }

    None
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn write_state_db(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.vscdb");
        let conn = Connection::open(&path).expect("open");
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .expect("create table");
        for (key, value) in entries {
            conn.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                [key, value],
            )
            .expect("insert");
        }
        dir
    }

    #[test]
    fn reads_access_token_and_email() {
        let dir = write_state_db(&[
            ("cursorAuth/accessToken", "eyJhbGciOiJIUzI1NiJ9.payload.sig"),
            ("cursorAuth/cachedEmail", "person@example.com"),
        ]);
        let creds = read_credentials_from(&dir.path().join("state.vscdb")).expect("creds");
        assert_eq!(creds.access_token, "eyJhbGciOiJIUzI1NiJ9.payload.sig");
        assert_eq!(creds.email.as_deref(), Some("person@example.com"));
    }

    #[test]
    fn strips_surrounding_json_quotes() {
        let dir = write_state_db(&[("cursorAuth/accessToken", "\"tok-123\"")]);
        let creds = read_credentials_from(&dir.path().join("state.vscdb")).expect("creds");
        assert_eq!(creds.access_token, "tok-123");
        assert!(creds.email.is_none());
    }

    #[test]
    fn missing_token_key_is_auth_required() {
        let dir = write_state_db(&[("someOther/key", "value")]);
        let err = read_credentials_from(&dir.path().join("state.vscdb")).expect_err("no token");
        assert!(matches!(err, ProviderError::AuthRequired));
    }

    #[test]
    fn missing_database_is_not_installed() {
        let err =
            read_credentials_from(Path::new("/nonexistent/cursor-state.vscdb")).expect_err("no db");
        assert!(matches!(err, ProviderError::NotInstalled(_)));
    }

    #[test]
    fn decodes_utf16_blob_value() {
        let json = "tok-utf16";
        let bytes: Vec<u8> = json.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        assert_eq!(decode_blob(&bytes).as_deref(), Some(json));
    }
}
