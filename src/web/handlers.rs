use super::{
    AppState, auth,
    forms::{self, FormData, NoticeParams},
    views::{AboutView, ErrorView, HostView, LoginView, OverviewView, PageData, VmSummary, VmView},
};
use askama::Template;
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde_json::json;
use std::path::Path;

pub async fn login_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<NoticeParams>,
) -> Response {
    if already_authenticated(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let error_visible = params.error.is_some();
    render(LoginView {
        error_visible,
        error_message: "Token 不正确，请重试。".into(),
        version: super::views::VERSION,
    })
}

pub async fn login_submit(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(form) = FormData::parse(&body) else {
        return Redirect::to("/login?error=1").into_response();
    };
    let supplied = form.first("token").unwrap_or_default().trim();
    if !auth::token_matches(&state.token, supplied) {
        return Redirect::to("/login?error=1").into_response();
    }
    let Ok(cookie) = auth::session_cookie(&state.token) else {
        return internal_error("无法生成登录 Cookie");
    };
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().append(header::SET_COOKIE, cookie);
    response
}

pub async fn logout(State(_state): State<AppState>) -> Response {
    let mut response = Redirect::to("/login").into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, auth::expired_cookie());
    response
}

pub async fn overview(
    State(state): State<AppState>,
    Query(params): Query<NoticeParams>,
) -> Response {
    match load_page_data("overview", &params, &state).await {
        Ok(data) => render(OverviewView::new(data)),
        Err(response) => response,
    }
}

pub async fn host_page(
    State(state): State<AppState>,
    Query(params): Query<NoticeParams>,
) -> Response {
    match load_page_data("host", &params, &state).await {
        Ok(data) => render(HostView::new(data)),
        Err(response) => response,
    }
}

pub async fn vm_page(
    State(state): State<AppState>,
    Query(params): Query<NoticeParams>,
) -> Response {
    match load_page_data("vm", &params, &state).await {
        Ok(data) => render(VmView::new(data)),
        Err(response) => response,
    }
}

pub async fn about(State(state): State<AppState>, Query(params): Query<NoticeParams>) -> Response {
    match load_page_data("about", &params, &state).await {
        Ok(data) => {
            let vm = data.vm();
            render(AboutView {
                chrome: data.chrome,
                host: data.host,
                vm,
            })
        }
        Err(response) => response,
    }
}

pub async fn api_status(State(state): State<AppState>) -> Response {
    let status_result = tokio::task::spawn_blocking(crate::libvirt::domain_status).await;
    let status = match status_result {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                json!({
                    "ok": false,
                    "error": error.to_string(),
                    "vm": {
                        "label": "无法读取",
                        "stateClass": "danger",
                        "active": false,
                        "defined": false
                    }
                })
                .to_string(),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                json!({"ok": false, "error": format!("状态任务失败：{error}")}).to_string(),
            )
                .into_response();
        }
    };
    let cpu_percent = state.sample_cpu(&status);
    let summary = VmSummary::from_status(&status, cpu_percent);
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        json!({
            "ok": true,
            "vm": {
                "label": summary.label,
                "stateClass": summary.state_class,
                "active": summary.active,
                "defined": summary.defined,
                "cpuCount": summary.cpu_count,
                "cpuPercent": summary.cpu_percent,
                "memoryCurrent": summary.memory_current,
                "memoryMax": summary.memory_max,
            }
        })
        .to_string(),
    )
        .into_response()
}

pub async fn save_configuration(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(_guard) = state.operation_lock.try_lock() else {
        return forms::redirect_notice("/vm", "已有操作正在进行，请等待完成。", true);
    };
    let form = match FormData::parse(&body) {
        Ok(form) => form,
        Err(error) => return forms::redirect_notice("/vm", error, true),
    };
    let action = form.first("action").unwrap_or("save").to_string();
    let host = match tokio::task::spawn_blocking(crate::host::inspect_host).await {
        Ok(host) => host,
        Err(error) => return internal_error(&format!("读取硬件信息失败：{error}")),
    };
    let current = crate::config::load();
    let config = match forms::parse_config(&form, &host, &current) {
        Ok(config) => config,
        Err(error) => return forms::redirect_notice("/vm", error, true),
    };
    if let Err(error) = forms::save_config(&config) {
        return forms::redirect_notice("/vm", error, true);
    }

    match action.as_str() {
        "save" => forms::redirect_notice("/vm", "配置已保存。", false),
        "define" => {
            let define_host = host.clone();
            let define_config = config.clone();
            let result = tokio::task::spawn_blocking(move || {
                define_vm(&define_config, &define_host)
                    .map(|()| "虚拟机定义已保存。现在可以启动并安装 Windows。".to_string())
                    .map_err(|error| error.to_string())
            })
            .await;
            action_result("/vm", result)
        }
        "create-disk" => {
            let path = config.disk_path.clone();
            let size = config.disk_size_gib;
            if Path::new(&path).exists() {
                return forms::redirect_notice("/vm", format!("虚拟磁盘已存在：{path}"), false);
            }
            if !path.starts_with(crate::config::IMAGE_ROOT) {
                return forms::redirect_notice(
                    "/vm",
                    "虚拟磁盘必须创建在 /var/lib/libvirt/images 下。",
                    true,
                );
            }
            let result = tokio::task::spawn_blocking(move || {
                crate::privileged::request_create_disk(Path::new(&path), size)
                    .map(|()| format!("虚拟磁盘已创建：{path}"))
                    .map_err(|error| error.to_string())
            })
            .await;
            action_result("/vm", result)
        }
        _ => forms::redirect_notice("/vm", "未知的虚拟机操作。", true),
    }
}

pub async fn apply_vfio(State(state): State<AppState>) -> Response {
    let Ok(_guard) = state.operation_lock.try_lock() else {
        return forms::redirect_notice("/host", "已有操作正在进行，请等待完成。", true);
    };
    let result = tokio::task::spawn_blocking(apply_vfio_blocking).await;
    action_result("/host", result)
}

pub async fn start_vm(State(state): State<AppState>) -> Response {
    let Ok(_guard) = state.operation_lock.try_lock() else {
        return forms::redirect_notice("/", "已有操作正在进行，请等待完成。", true);
    };
    let result = tokio::task::spawn_blocking(|| {
        let config = crate::config::load();
        crate::libvirt::validate_vm_start(&config).map_err(|error| error.to_string())?;
        crate::libvirt::run_control("start").map_err(|error| error.to_string())?;
        Ok::<_, String>("虚拟机正在启动。请确认 NVIDIA 输出口已连接显示器。".into())
    })
    .await;
    action_result("/", result)
}

pub async fn shutdown_vm(State(state): State<AppState>) -> Response {
    let Ok(_guard) = state.operation_lock.try_lock() else {
        return forms::redirect_notice("/", "已有操作正在进行，请等待完成。", true);
    };
    let result = tokio::task::spawn_blocking(|| {
        crate::libvirt::run_control("shutdown")
            .map(|_| "已发送正常关机请求（ACPI）。".to_string())
            .map_err(|error| error.to_string())
    })
    .await;
    action_result("/", result)
}

pub async fn force_stop_vm(State(state): State<AppState>) -> Response {
    let Ok(_guard) = state.operation_lock.try_lock() else {
        return forms::redirect_notice("/", "已有操作正在进行，请等待完成。", true);
    };
    let result = tokio::task::spawn_blocking(|| {
        crate::libvirt::run_control("destroy")
            .map(|_| "虚拟机已强制停止。".to_string())
            .map_err(|error| error.to_string())
    })
    .await;
    action_result("/", result)
}

pub async fn undefine_vm(State(state): State<AppState>) -> Response {
    let Ok(_guard) = state.operation_lock.try_lock() else {
        return forms::redirect_notice("/vm", "已有操作正在进行，请等待完成。", true);
    };
    let result = tokio::task::spawn_blocking(|| {
        let status = crate::libvirt::domain_status().map_err(|error| error.to_string())?;
        if status.state.is_active() {
            return Err("请先停止虚拟机，再删除它的定义。".to_string());
        }
        crate::libvirt::undefine_domain().map_err(|error| error.to_string())?;
        Ok::<_, String>("虚拟机定义已删除。虚拟磁盘和 ISO 未被删除。".into())
    })
    .await;
    action_result("/vm", result)
}

pub async fn import_media(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(_guard) = state.operation_lock.try_lock() else {
        return forms::redirect_notice("/vm", "已有操作正在进行，请等待完成。", true);
    };
    let form = match FormData::parse(&body) {
        Ok(form) => form,
        Err(error) => return forms::redirect_notice("/vm", error, true),
    };
    let kind = form.first("kind").unwrap_or_default().to_string();
    if !matches!(kind.as_str(), "windows" | "virtio") {
        return forms::redirect_notice("/vm", "未知的介质类型。", true);
    }
    let source = form.first("source").unwrap_or_default().trim().to_string();
    if source.is_empty() {
        return forms::redirect_notice("/vm", "请填写服务器可见的 ISO 路径。", true);
    }
    let result = tokio::task::spawn_blocking(move || import_media_blocking(&kind, &source)).await;
    action_result("/vm", result)
}

pub async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        render(ErrorView {
            status: 404,
            title: "页面不存在".into(),
            message: "请求的页面不存在，或地址已经变化。".into(),
            version: super::views::VERSION,
        }),
    )
        .into_response()
}

async fn load_page_data(
    active: &'static str,
    params: &NoticeParams,
    state: &AppState,
) -> Result<PageData, Response> {
    let (host_result, status_result) = tokio::join!(
        tokio::task::spawn_blocking(crate::host::inspect_host),
        tokio::task::spawn_blocking(crate::libvirt::domain_status)
    );
    let host =
        host_result.map_err(|error| internal_error(&format!("读取硬件信息失败：{error}")))?;
    let status = match status_result {
        Ok(result) => result,
        Err(error) => {
            return Err(internal_error(&format!("读取虚拟机状态失败：{error}")));
        }
    };
    let cpu_percent = status
        .as_ref()
        .ok()
        .and_then(|status| state.sample_cpu(status));
    Ok(PageData::new(
        active,
        params,
        host,
        crate::config::load(),
        status,
        cpu_percent,
    ))
}

fn define_vm(
    config: &crate::model::VmConfig,
    host: &crate::model::HostSnapshot,
) -> Result<(), String> {
    let status = crate::libvirt::domain_status().map_err(|error| error.to_string())?;
    if status.state.is_active() {
        return Err("请先在 Windows 中正常关机，再保存硬件配置。".into());
    }
    let updating = !matches!(status.state, crate::libvirt::PowerState::NotDefined);
    if updating {
        config.validate().map_err(|error| error.to_string())?;
    } else {
        config
            .validate_for_create()
            .map_err(|error| error.to_string())?;
    }
    crate::libvirt::validate_install_media(config).map_err(|error| error.to_string())?;
    crate::libvirt::validate_physical_disks(config).map_err(|error| error.to_string())?;
    host.validate_pci_passthrough(config)
        .map_err(|error| error.to_string())?;
    if !Path::new(&config.disk_path).is_file() {
        return Err("虚拟磁盘尚未创建，请先创建虚拟磁盘。".into());
    }

    let mut config = config.clone();
    if config.vm_uuid.is_empty() {
        config.vm_uuid = if updating {
            crate::libvirt::domain_uuid()
                .map_err(|error| error.to_string())?
                .unwrap_or_default()
        } else {
            String::new()
        };
        if config.vm_uuid.is_empty() {
            config.vm_uuid = crate::libvirt::new_uuid().map_err(|error| error.to_string())?;
        }
    }
    let xml = crate::libvirt::generate_domain_xml(&config, &host.firmware)
        .map_err(|error| error.to_string())?;
    crate::libvirt::define_domain(&xml).map_err(|error| error.to_string())?;
    forms::save_config(&config)?;
    Ok(())
}

fn apply_vfio_blocking() -> Result<String, String> {
    let status = crate::libvirt::domain_status().map_err(|error| error.to_string())?;
    if status.state.is_active() {
        return Err("请先关闭虚拟机，再修改 VFIO 绑定。".into());
    }
    let host = crate::host::inspect_host();
    if host.secure_boot_enabled == Some(true) {
        return Err("Secure Boot 已启用，请先在固件设置中关闭。".into());
    }
    let config = crate::config::load();
    let ids = host
        .selected_vfio_ids(&config)
        .map_err(|error| error.to_string())?;
    if ids.is_empty() {
        return Err("请先在虚拟机页面选择要直通的 PCI 设备。".into());
    }
    let conflicts = host
        .selected_vfio_conflicts(&config)
        .map_err(|error| error.to_string())?;
    if !conflicts.is_empty() {
        return Err(format!(
            "vfio-pci ID 也会绑定未选择的设备：{}。请调整设备选择。",
            conflicts.join(", ")
        ));
    }
    host.validate_pci_passthrough(&config)
        .map_err(|error| error.to_string())?;
    crate::privileged::request_vfio_apply(&ids)
        .map(|_| "VFIO 配置已写入，initramfs 已更新。请重启主机使绑定生效。".to_string())
        .map_err(|error| error.to_string())
}

fn import_media_blocking(kind: &str, source: &str) -> Result<String, String> {
    let source_path = Path::new(source);
    if !source_path.is_file() {
        return Err(format!("文件不存在：{source}"));
    }
    let mut config = crate::config::load();
    if crate::config::qemu_can_read(source) {
        set_media_path(&mut config, kind, source.to_string())?;
        forms::save_config(&config)?;
        return Ok(format!("已选择 QEMU 可读介质：{source}"));
    }
    let Some(file_name) = source_path.file_name() else {
        return Err("无法确定 ISO 文件名。".into());
    };
    let destination = crate::config::media_dir().join(file_name);
    let destination_text = destination.to_string_lossy().into_owned();
    if !destination.is_file() {
        crate::privileged::request_import_media(source_path, &destination)
            .map_err(|error| error.to_string())?;
    }
    set_media_path(&mut config, kind, destination_text.clone())?;
    forms::save_config(&config)?;
    Ok(format!("ISO 已导入媒体库：{destination_text}"))
}

fn set_media_path(
    config: &mut crate::model::VmConfig,
    kind: &str,
    path: String,
) -> Result<(), String> {
    match kind {
        "windows" => config.windows_iso = path,
        "virtio" => config.virtio_iso = path,
        _ => return Err("未知的介质类型。".into()),
    }
    Ok(())
}

fn action_result(
    path: &str,
    result: Result<Result<String, String>, tokio::task::JoinError>,
) -> Response {
    match result {
        Ok(Ok(message)) => forms::redirect_notice(path, message, false),
        Ok(Err(error)) => forms::redirect_notice(path, error, true),
        Err(error) => forms::redirect_notice(path, format!("后台任务失败：{error}"), true),
    }
}

fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            eprintln!("Template rendering failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>Winship 模板渲染失败</h1>".to_string()),
            )
                .into_response()
        }
    }
}

fn internal_error(message: &str) -> Response {
    eprintln!("Winship error: {message}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        render(ErrorView {
            status: 500,
            title: "内部错误".into(),
            message: message.to_string(),
            version: super::views::VERSION,
        }),
    )
        .into_response()
}

fn already_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    auth::cookie_value(headers, auth::COOKIE_NAME)
        .is_some_and(|token| auth::token_matches(&state.token, &token))
}
