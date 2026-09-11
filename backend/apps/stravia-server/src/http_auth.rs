use crate::admin_entry::{RequestOrigin, canonical_origin};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use stravia_core::admin::identity::{AdminAuth, AuthError, SessionTokens};

use crate::AdminMode;

const ACCESS_COOKIE: &str = "stravia_access";
const REFRESH_COOKIE: &str = "stravia_refresh";
const CSRF_HEADER: &str = "x-stravia-csrf";

#[derive(Clone)]
pub(crate) struct AdminHttpState {
    pub auth: AdminAuth,
    pub mode: AdminMode,
}

#[derive(Serialize)]
struct AuthStateResponse {
    mode: &'static str,
    authenticated: bool,
    setup_authorized: bool,
    username: Option<String>,
}

#[derive(Deserialize)]
struct LoginInput {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct CredentialsInput {
    current_password: String,
    username: String,
    password: String,
}

#[derive(Serialize)]
struct SessionResponse {
    username: Option<String>,
    access_expires_at: i64,
    session_expires_at: i64,
}

pub(crate) fn auth_router(state: AdminHttpState) -> Router {
    Router::new()
        .route("/api/v1/auth/state", get(auth_state))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/refresh", post(refresh))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/credentials", put(change_credentials))
        .with_state(state)
}

pub(crate) async fn require_admin(
    State(state): State<AdminHttpState>,
    mut request: Request,
    next: Next,
) -> Response {
    if state.mode == AdminMode::Server && request.method() != Method::GET {
        if let Err(response) = validate_web_request(
            request
                .extensions()
                .get::<RequestOrigin>()
                .map(RequestOrigin::as_str),
            request.headers(),
            false,
        ) {
            return response;
        }
    }

    let token = match state.mode {
        AdminMode::Desktop => bearer_token(request.headers()),
        AdminMode::Server => cookie(request.headers(), ACCESS_COOKIE),
    };
    let Some(token) = token else {
        return auth_error(StatusCode::UNAUTHORIZED, "unauthorized");
    };

    match state.auth.authenticate(token).await {
        Ok(session) => {
            request.extensions_mut().insert(session);
            next.run(request).await
        }
        Err(error) => map_auth_error(error),
    }
}

async fn auth_state(State(state): State<AdminHttpState>, headers: HeaderMap) -> Response {
    let token = match state.mode {
        AdminMode::Desktop => bearer_token(&headers),
        AdminMode::Server => cookie(&headers, ACCESS_COOKIE),
    };
    let session = match token {
        Some(token) => match state.auth.authenticate(token).await {
            Ok(session) => Some(session),
            Err(AuthError::Unauthorized) => None,
            Err(error) => return map_auth_error(error),
        },
        None => None,
    };
    Json(AuthStateResponse {
        mode: match state.mode {
            AdminMode::Server => "server",
            AdminMode::Desktop => "desktop",
        },
        authenticated: session.is_some(),
        setup_authorized: false,
        username: session.and_then(|value| value.username),
    })
    .into_response()
}

async fn login(
    State(state): State<AdminHttpState>,
    origin: Option<Extension<RequestOrigin>>,
    headers: HeaderMap,
    Json(input): Json<LoginInput>,
) -> Response {
    if state.mode != AdminMode::Server {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Err(response) = validate_web_request(
        origin.as_ref().map(|origin| origin.as_str()),
        &headers,
        true,
    ) {
        return response;
    }
    match state.auth.login(&input.username, &input.password).await {
        Ok(tokens) => {
            session_response(
                &state,
                origin.as_ref().is_some_and(|origin| origin.secure()),
                tokens,
            )
            .await
        }
        Err(AuthError::InvalidCredentials) => {
            auth_error(StatusCode::UNAUTHORIZED, "invalid_credentials")
        }
        Err(error) => map_auth_error(error),
    }
}

async fn refresh(
    State(state): State<AdminHttpState>,
    origin: Option<Extension<RequestOrigin>>,
    headers: HeaderMap,
) -> Response {
    if state.mode != AdminMode::Server {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Err(response) = validate_web_request(
        origin.as_ref().map(|origin| origin.as_str()),
        &headers,
        false,
    ) {
        return response;
    }
    let Some(token) = cookie(&headers, REFRESH_COOKIE) else {
        return auth_error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    match state.auth.refresh(token).await {
        Ok(tokens) => {
            session_response(
                &state,
                origin.as_ref().is_some_and(|origin| origin.secure()),
                tokens,
            )
            .await
        }
        Err(error) => map_auth_error(error),
    }
}

async fn logout(
    State(state): State<AdminHttpState>,
    origin: Option<Extension<RequestOrigin>>,
    headers: HeaderMap,
) -> Response {
    if state.mode != AdminMode::Server {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Err(response) = validate_web_request(
        origin.as_ref().map(|origin| origin.as_str()),
        &headers,
        false,
    ) {
        return response;
    }
    let session_id = if let Some(token) = cookie(&headers, ACCESS_COOKIE) {
        match state.auth.authenticate(token).await {
            Ok(session) => Some(session.session_id),
            Err(AuthError::Unauthorized) => None,
            Err(error) => return map_auth_error(error),
        }
    } else {
        None
    };
    let session_id = match session_id {
        Some(session_id) => Some(session_id),
        None => match cookie(&headers, REFRESH_COOKIE) {
            Some(token) => match state.auth.refresh(token).await {
                Ok(tokens) => Some(tokens.session_id),
                Err(AuthError::Unauthorized) => None,
                Err(error) => return map_auth_error(error),
            },
            None => None,
        },
    };
    if let Some(session_id) = session_id
        && let Err(error) = state.auth.logout(&session_id).await
    {
        return map_auth_error(error);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    clear_session_cookies(
        origin.as_ref().is_some_and(|origin| origin.secure()),
        response.headers_mut(),
    );
    response
}

async fn change_credentials(
    State(state): State<AdminHttpState>,
    origin: Option<Extension<RequestOrigin>>,
    headers: HeaderMap,
    Json(input): Json<CredentialsInput>,
) -> Response {
    if state.mode != AdminMode::Server {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Err(response) = validate_web_request(
        origin.as_ref().map(|origin| origin.as_str()),
        &headers,
        true,
    ) {
        return response;
    }
    let Some(token) = cookie(&headers, ACCESS_COOKIE) else {
        return auth_error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let session = match state.auth.authenticate(token).await {
        Ok(session) => session,
        Err(error) => return map_auth_error(error),
    };
    match state
        .auth
        .change_credentials(
            &session,
            &input.current_password,
            &input.username,
            &input.password,
        )
        .await
    {
        Ok(()) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            clear_session_cookies(
                origin.as_ref().is_some_and(|origin| origin.secure()),
                response.headers_mut(),
            );
            response
        }
        Err(AuthError::InvalidCredentials) => {
            auth_error(StatusCode::UNAUTHORIZED, "invalid_credentials")
        }
        Err(error) => map_auth_error(error),
    }
}

async fn session_response(state: &AdminHttpState, secure: bool, tokens: SessionTokens) -> Response {
    let username = state
        .auth
        .authenticate(&tokens.access_token)
        .await
        .ok()
        .and_then(|session| session.username);
    let body = SessionResponse {
        username,
        access_expires_at: tokens.access_expires_at,
        session_expires_at: tokens.session_expires_at,
    };
    let mut response = Json(body).into_response();
    append_cookie(
        response.headers_mut(),
        ACCESS_COOKIE,
        &tokens.access_token,
        "/",
        secure,
        (tokens.access_expires_at - chrono::Utc::now().timestamp()).max(0),
    );
    append_cookie(
        response.headers_mut(),
        REFRESH_COOKIE,
        &tokens.refresh_token,
        "/api/v1/auth",
        secure,
        (tokens.session_expires_at - chrono::Utc::now().timestamp()).max(0),
    );
    response
}

pub(crate) fn validate_web_request(
    expected_origin: Option<&str>,
    headers: &HeaderMap,
    json_body: bool,
) -> Result<(), Response> {
    let Some(expected_origin) = expected_origin else {
        return Err(auth_error(StatusCode::FORBIDDEN, "origin_required"));
    };
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let origin = origins
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|value| canonical_origin(value).ok());
    if origins.next().is_some() || origin.as_deref() != Some(expected_origin) {
        return Err(auth_error(StatusCode::FORBIDDEN, "origin_mismatch"));
    }
    if headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(auth_error(StatusCode::FORBIDDEN, "csrf_required"));
    }
    if json_body
        && !headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|value| {
                value.eq_ignore_ascii_case("application/json")
                    || value.to_ascii_lowercase().starts_with("application/json;")
            })
    {
        return Err(auth_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "json_required",
        ));
    }
    Ok(())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|value| !value.is_empty())
}

pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(name)?.strip_prefix('='))
        .filter(|value| !value.is_empty())
}

fn append_cookie(
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
    path: &str,
    secure: bool,
    max_age: i64,
) {
    let secure = if secure { "; Secure" } else { "" };
    let value = format!(
        "{name}={value}; Path={path}; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.append(header::SET_COOKIE, value);
    }
}

fn clear_session_cookies(secure: bool, headers: &mut HeaderMap) {
    for (name, path) in [(ACCESS_COOKIE, "/"), (REFRESH_COOKIE, "/api/v1/auth")] {
        let secure = if secure { "; Secure" } else { "" };
        let value = format!("{name}=; Path={path}; HttpOnly; SameSite=Strict; Max-Age=0{secure}");
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.append(header::SET_COOKIE, value);
        }
    }
}

fn map_auth_error(error: AuthError) -> Response {
    match error {
        AuthError::InvalidCredentials => {
            auth_error(StatusCode::UNAUTHORIZED, "invalid_credentials")
        }
        AuthError::Unauthorized => auth_error(StatusCode::UNAUTHORIZED, "unauthorized"),
        AuthError::Conflict => auth_error(StatusCode::CONFLICT, "conflict"),
        AuthError::InvalidInput(_) => auth_error(StatusCode::BAD_REQUEST, "invalid_input"),
        AuthError::Storage(_) => auth_error(StatusCode::SERVICE_UNAVAILABLE, "auth_unavailable"),
    }
}

pub(crate) fn auth_error(status: StatusCode, code: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": code, "code": code })),
    )
        .into_response()
}
