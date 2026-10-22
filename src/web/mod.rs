mod auth;
mod forms;
mod handlers;
mod views;

use axum::{
    Router,
    body::Body,
    extract::DefaultBodyLimit,
    http::{HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{sync::Arc, time::Instant};
use tokio::sync::Mutex;
use tower_http::compression::CompressionLayer;

const APP_CSS: &str = include_str!("../../static/app.css");
const APP_JS: &str = include_str!("../../static/app.js");
const FAVICON: &str = include_str!("../../static/favicon.svg");

#[derive(Debug, Clone, Copy)]
pub struct CpuSample {
    pub sampled_at: Instant,
    pub cpu_time_ns: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub token: Arc<str>,
    pub operation_lock: Arc<Mutex<()>>,
    pub cpu_sample: Arc<std::sync::Mutex<Option<CpuSample>>>,
}

impl AppState {
    fn new(token: String) -> Self {
        Self {
            token: Arc::from(token),
            operation_lock: Arc::new(Mutex::new(())),
            cpu_sample: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn sample_cpu(&self, status: &crate::libvirt::VmStatus) -> Option<f32> {
        let Some(cpu_time) = status.cpu_time_ns else {
            if let Ok(mut sample) = self.cpu_sample.lock() {
                *sample = None;
            }
            return None;
        };
        let now = Instant::now();
        let mut sample = self.cpu_sample.lock().ok()?;
        let result = sample.map(|previous| {
            let elapsed = now.duration_since(previous.sampled_at).as_nanos().max(1) as f64;
            let cpus = status.cpu_count.unwrap_or(1).max(1) as f64;
            let delta = cpu_time.saturating_sub(previous.cpu_time_ns) as f64;
            ((delta / elapsed) * 100.0 / cpus).clamp(0.0, 100.0) as f32
        });
        *sample = Some(CpuSample {
            sampled_at: now,
            cpu_time_ns: cpu_time,
        });
        result
    }
}

pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let token_path = auth::token_file_path();
    let token = auth::load_or_create_token().map_err(|error| {
        format!(
            "could not load or create the access token at {}: {error}. Run Winship as a user that can write /etc/winship or set WINSHIP_TOKEN_FILE for local development",
            token_path.display()
        )
    })?;
    let state = AppState::new(token);
    let protected = Router::new()
        .route("/", get(handlers::overview))
        .route("/host", get(handlers::host_page))
        .route("/vm", get(handlers::vm_page))
        .route("/about", get(handlers::about))
        .route("/logout", post(handlers::logout))
        .route("/api/status", get(handlers::api_status))
        .route("/host/apply-vfio", post(handlers::apply_vfio))
        .route("/vm/config", post(handlers::save_configuration))
        .route("/vm/action/start", post(handlers::start_vm))
        .route("/vm/action/shutdown", post(handlers::shutdown_vm))
        .route("/vm/action/force-stop", post(handlers::force_stop_vm))
        .route("/vm/action/undefine", post(handlers::undefine_vm))
        .route("/vm/import-media", post(handlers::import_media))
        .fallback(handlers::not_found)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let app = Router::new()
        .route(
            "/login",
            get(handlers::login_page).post(handlers::login_submit),
        )
        .route("/static/app.css", get(styles))
        .route("/static/app.js", get(script))
        .route("/favicon.svg", get(favicon))
        .merge(protected)
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn(security_headers))
        .with_state(state);

    let listen = std::env::var("WINSHIP_LISTEN").unwrap_or_else(|_| "0.0.0.0:15277".into());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|error| format!("could not listen on {listen}: {error}"))?;
    let address = listener.local_addr()?;
    eprintln!("Winship listening on http://{address}");
    eprintln!("Access token: {}", token_path.display());
    if address.ip().is_unspecified() {
        eprintln!(
            "The service is reachable from the network; every page and API requires the token."
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn styles() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], FAVICON)
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let status = response.status();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; style-src 'self'; script-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        axum::http::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    if status == StatusCode::UNAUTHORIZED {
        headers.insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"Winship\""),
        );
    }
    response
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            eprintln!("Could not install Ctrl+C handler: {error}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => eprintln!("Could not install SIGTERM handler: {error}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
