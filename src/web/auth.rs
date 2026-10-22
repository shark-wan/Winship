use super::AppState;
use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use rand::RngCore;
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const COOKIE_NAME: &str = "winship_token";

pub fn token_file_path() -> PathBuf {
    std::env::var_os("WINSHIP_TOKEN_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/winship/token"))
}

pub fn load_or_create_token() -> io::Result<String> {
    let path = token_file_path();
    if path.exists() {
        return read_token(&path);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("/etc/winship"));
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;

    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let mut token = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }

    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            Ok(token)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_token(&path),
        Err(error) => Err(error),
    }
}

fn read_token(path: &Path) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let token = fs::read_to_string(path)?.trim().to_string();
    if token.len() < 32 || token.bytes().any(|byte| !byte.is_ascii_graphic()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} does not contain a valid Winship token; remove it and restart Winship to generate a new one",
                path.display()
            ),
        ));
    }
    Ok(token)
}

pub fn token_matches(expected: &str, supplied: &str) -> bool {
    let left = expected.as_bytes();
    let right = supplied.as_bytes();
    let mut difference = u8::from(left.len() != right.len());
    for index in 0..left.len().max(right.len()) {
        difference |= left.get(index).copied().unwrap_or_default()
            ^ right.get(index).copied().unwrap_or_default();
    }
    difference == 0
}

pub fn session_cookie(token: &str) -> Result<HeaderValue, axum::http::header::InvalidHeaderValue> {
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000"
    ))
}

pub fn expired_cookie() -> HeaderValue {
    HeaderValue::from_static("winship_token=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

pub async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request_is_authenticated(&request, &state.token) {
        return next.run(request).await;
    }

    if request.uri().path().starts_with("/api/") {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            r#"{"error":"authentication required"}"#,
        )
            .into_response();
    }
    Redirect::to("/login").into_response()
}

fn request_is_authenticated(request: &Request, expected: &str) -> bool {
    if let Some(token) = cookie_value(request.headers(), COOKIE_NAME) {
        if token_matches(expected, &token) {
            return true;
        }
    }
    if let Some(value) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        if let Some(token) = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
        {
            return token_matches(expected, token.trim());
        }
    }
    false
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_tokens_without_early_exit() {
        assert!(token_matches("0123456789abcdef", "0123456789abcdef"));
        assert!(!token_matches("0123456789abcdef", "0123456789abcdee"));
        assert!(!token_matches("short", "a-much-longer-value"));
    }

    #[test]
    fn reads_named_cookie() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; winship_token=abc; other=1"),
        );
        assert_eq!(cookie_value(&headers, COOKIE_NAME).as_deref(), Some("abc"));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }
}
