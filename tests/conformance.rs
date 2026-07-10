//! cr-301: 契约测试入口
//!
//! 把 `tests/conformance/common/` 当成一个 module 引用，触发 L2/L3 测试。
//! 具体的子测试文件直接 cargo test 各自运行（因为 #[path] 引用）。

#[path = "conformance/common/mod.rs"]
mod common;

#[path = "conformance/openai_protocol.rs"]
mod openai_protocol;

#[path = "conformance/metrics_endpoint.rs"]
mod metrics_endpoint;

#[path = "conformance/admin_auth.rs"]
mod admin_auth;
