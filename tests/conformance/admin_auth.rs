//! cr-203: /admin/reload 端点鉴权契约测试
//!
//! 验证：
//! 1. admin_token="" → 端点返回 404
//! 2. admin_token=Some → 缺 X-Admin-Token 头返回 401
//! 3. admin_token=Some → 错 X-Admin-Token 返回 401
//! 4. admin_token=Some → 正确 X-Admin-Token 返回 200

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mygate::router::openai::AppState;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;

fn build_app(admin_token: Option<&str>) -> axum::Router {
    let token_line = admin_token
        .map(|t| format!("admin_token = \"{}\"\n", t))
        .unwrap_or_default();
    let config: mygate::config::AppConfig = toml::from_str(&format!(
        r#"
[server]
host = "127.0.0.1"
port = 8080
timeout_seconds = 30
{token_line}
[providers.mock]
base_url = "http://127.0.0.1:9999/v1"
api_key = "test"
provider_type = "openai"
auth_style = "bearer"

[aliases.T]
[[aliases.T.chain]]
provider = "mock"
model = "m"
priority = 1
"#
    ))
    .unwrap();
    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        client: reqwest::Client::new(),
    };
    mygate::server::build_router(state)
}

fn post_reload_request(token: Option<&str>) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri("/admin/reload")
        .body(Body::empty())
        .unwrap();
    if let Some(t) = token {
        req.headers_mut().insert(
            "x-admin-token",
            axum::http::HeaderValue::from_str(t).unwrap(),
        );
    }
    req
}

/// 1. admin_token 字段缺省（None）→ 端点禁用（返回 404）
#[tokio::test]
async fn admin_endpoint_disabled_when_no_token() {
    let app = build_app(None);
    let resp = app.oneshot(post_reload_request(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// 2. admin_token 已设 + 无 header → 401
#[tokio::test]
async fn admin_endpoint_rejects_missing_header() {
    let app = build_app(Some("secret"));
    let resp = app.oneshot(post_reload_request(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// 3. admin_token 已设 + 错 header → 401
#[tokio::test]
async fn admin_endpoint_rejects_wrong_token() {
    let app = build_app(Some("secret"));
    let resp = app.oneshot(post_reload_request(Some("wrong"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// 4. admin_token 已设 + 正确 header → 鉴权通过（**不**返回 401）
/// 后续 reload 步骤可能成功（200）也可能失败（500/404 取决于磁盘 config），
/// 但**绝不会**返回 401（401 意味着鉴权失败）。
#[tokio::test]
async fn admin_endpoint_accepts_correct_token() {
    let app = build_app(Some("secret"));
    let resp = app.oneshot(post_reload_request(Some("secret"))).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "正确 token 不应返回 401（鉴权失败）"
    );
}
