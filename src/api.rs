use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State, rejection::JsonRejection, ws::WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{delete, get, put},
};
use serde::{Deserialize, Serialize};
use tower_http::{cors::CorsLayer, set_header::SetResponseHeaderLayer, trace::TraceLayer};

use crate::{
    config::{ApplicationProfile, LocalApplication},
    drivers, platform,
    service::{BatteryReading, BridgeService},
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiResult {
    ok: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutostartPayload {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfilesPayload {
    profiles: Vec<ApplicationProfile>,
}

#[derive(Debug, Deserialize)]
struct LocalApplicationsPayload {
    applications: Vec<LocalApplication>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeSettingsPayload {
    brand: String,
    dpi: Option<u32>,
    polling_rate_hz: Option<u32>,
}

pub fn router(service: BridgeService, origins: &[String]) -> Router {
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    let request_origins = Arc::new(origins.iter().cloned().collect::<HashSet<_>>());
    let hid_origins = Arc::clone(&request_origins);
    let hid_route = Router::new().route(
        "/v1/hid",
        get(move |websocket, headers| hid_upgrade(websocket, headers, Arc::clone(&hid_origins))),
    );
    let cors = CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([Method::GET, Method::PUT, Method::DELETE])
        .allow_headers([axum::http::header::CONTENT_TYPE])
        .max_age(Duration::from_secs(3600));
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/handshake", put(handshake))
        .route("/v1/games", get(games))
        .route(
            "/v1/local-applications",
            get(local_applications).put(replace_local_applications),
        )
        .route("/v1/applications", get(applications))
        .route("/v1/applications/{icon_id}/icon", get(application_icon))
        .route("/v1/running-applications", get(running_applications))
        .route("/v1/profiles", get(profiles).put(replace_profiles))
        .route("/v1/default-profile", put(set_default_profile))
        .route("/v1/native/settings", put(apply_native_settings))
        .route("/v1/battery", put(record_battery))
        .route("/v1/battery/{device_id}", delete(forget_battery))
        .route("/v1/autostart", put(set_autostart))
        .route("/v1/panel", put(show_panel))
        .merge(hid_route)
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::HeaderName::from_static("access-control-allow-private-network"),
            HeaderValue::from_static("true"),
        ))
        .layer(cors)
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            request_gate(request, next, Arc::clone(&request_origins))
        }))
        .layer(TraceLayer::new_for_http())
        .with_state(service)
}

fn single_header(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

fn loopback_authority(host: &str) -> bool {
    let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let name = authority.host();
    if !matches!(name, "localhost" | "127.0.0.1" | "[::1]") {
        return false;
    }
    if host == name {
        return true;
    }
    let Some(port) = host
        .strip_prefix(name)
        .and_then(|value| value.strip_prefix(':'))
    else {
        return false;
    };
    !port.is_empty()
        && port.bytes().all(|value| value.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|value| value != 0)
}

async fn request_gate(
    request: Request,
    next: Next,
    allowed: Arc<HashSet<String>>,
) -> axum::response::Response {
    let headers = request.headers();
    let Some(host) = single_header(headers, header::HOST).filter(|host| loopback_authority(host))
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if request
        .uri()
        .authority()
        .is_some_and(|authority| authority.as_str() != host)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if headers.contains_key(header::ORIGIN)
        && !single_header(headers, header::ORIGIN)
            .is_some_and(|origin| origin != "null" && allowed.contains(origin))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn hid_upgrade(
    websocket: WebSocketUpgrade,
    headers: HeaderMap,
    allowed_origins: Arc<HashSet<String>>,
) -> Result<Response<Body>, StatusCode> {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::warn!("Rejected native HID WebSocket without an Origin header");
        return Err(StatusCode::FORBIDDEN);
    };
    if !allowed_origins.contains(origin) {
        tracing::warn!(%origin, "Rejected native HID WebSocket from an unapproved origin");
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(websocket
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(crate::hid::serve))
}

async fn status(State(service): State<BridgeService>) -> Json<crate::service::BridgeSnapshot> {
    Json(service.snapshot().await)
}

async fn show_panel(State(service): State<BridgeService>) -> Json<ApiResult> {
    service.request_show_panel();
    Json(ApiResult { ok: true })
}

async fn handshake(State(service): State<BridgeService>) -> Json<ApiResult> {
    service.record_client_heartbeat().await;
    Json(ApiResult { ok: true })
}

async fn applications(
    State(service): State<BridgeService>,
) -> Json<Vec<crate::applications::ApplicationInfo>> {
    Json(service.applications().await)
}

async fn running_applications(
    State(service): State<BridgeService>,
) -> Json<Vec<crate::applications::ApplicationInfo>> {
    Json(service.running_applications().await)
}

async fn application_icon(
    State(service): State<BridgeService>,
    Path(icon_id): Path<String>,
) -> Result<Response<Body>, StatusCode> {
    let icon = service
        .application_icon(&icon_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "private, max-age=86400")
        .body(Body::from(icon))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn profiles(State(service): State<BridgeService>) -> Json<Vec<ApplicationProfile>> {
    Json(service.profiles().await)
}

async fn games(State(service): State<BridgeService>) -> Json<Vec<serde_json::Value>> {
    Json(service.games().await)
}

async fn local_applications(State(service): State<BridgeService>) -> Json<Vec<LocalApplication>> {
    Json(service.local_applications().await)
}

async fn replace_local_applications(
    State(service): State<BridgeService>,
    payload: Result<Json<LocalApplicationsPayload>, JsonRejection>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let Json(payload) = payload.map_err(invalid_json)?;
    let applications = service
        .replace_local_applications(payload.applications)
        .await
        .map_err(profile_error)?;
    Ok(Json(
        serde_json::json!({"ok":true,"applications":applications}),
    ))
}

async fn replace_profiles(
    State(service): State<BridgeService>,
    payload: Result<Json<ProfilesPayload>, JsonRejection>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    let Json(payload) = payload.map_err(invalid_json)?;
    service
        .replace_profiles(payload.profiles)
        .await
        .map_err(profile_error)?;
    Ok(Json(ApiResult { ok: true }))
}

async fn set_default_profile(
    State(service): State<BridgeService>,
    payload: Result<Json<ApplicationProfile>, JsonRejection>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    let Json(profile) = payload.map_err(invalid_json)?;
    service
        .set_default_profile(profile)
        .await
        .map_err(profile_error)?;
    Ok(Json(ApiResult { ok: true }))
}

async fn record_battery(
    State(service): State<BridgeService>,
    Json(reading): Json<BatteryReading>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    service
        .record_battery(reading)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiResult { ok: true }))
}

async fn forget_battery(
    State(service): State<BridgeService>,
    Path(device_id): Path<String>,
) -> Json<ApiResult> {
    service.forget_battery(&device_id).await;
    Json(ApiResult { ok: true })
}

async fn set_autostart(
    Json(payload): Json<AutostartPayload>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    platform::set_autostart(payload.enabled).map_err(internal_error)?;
    Ok(Json(ApiResult { ok: true }))
}

static NATIVE_SETTINGS_ADMISSION: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

async fn apply_native_settings(
    Json(payload): Json<NativeSettingsPayload>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    if payload.dpi.is_none() && payload.polling_rate_hz.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "at least one of dpi or pollingRateHz is required".into(),
        ));
    }

    let permit = NATIVE_SETTINGS_ADMISSION
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "native settings are busy".into(),
            )
        })?;
    if crate::hid::client_session_active() {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "native HID is owned by an active WebSocket session".into(),
        ));
    }
    let NativeSettingsPayload {
        brand,
        dpi,
        polling_rate_hz,
    } = payload;
    let error_brand = brand.clone();
    let applied = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        drivers::apply_settings(&brand, dpi, polling_rate_hz)
    })
    .await
    .map_err(|error| internal_error(anyhow::anyhow!(error)))?
    .map_err(|error| {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
        {
            (StatusCode::TOO_MANY_REQUESTS, error.to_string())
        } else {
            internal_error(error)
        }
    })?;

    if !applied {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no native driver is registered for {error_brand}"),
        ));
    }
    Ok(Json(ApiResult { ok: true }))
}

fn invalid_json(error: JsonRejection) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, error.body_text())
}

fn profile_error(error: anyhow::Error) -> (StatusCode, String) {
    if error
        .downcast_ref::<crate::local_targets::InvalidInput>()
        .is_some()
    {
        (StatusCode::BAD_REQUEST, error.to_string())
    } else {
        internal_error(error)
    }
}

fn internal_error(error: anyhow::Error) -> (StatusCode, String) {
    tracing::error!(%error, "Bridge request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
