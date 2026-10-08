use anyhow::Result;
use axum::{
    Router,
    extract::Path,
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::process::{Command, Output};
use tokio::{net::TcpListener, task::JoinHandle};

const HTML: &str =
    "<!doctype html><title>HTTP fixture</title><h1>HTTP fixture</h1><p>response body</p>";
const RAW_BODY: &[u8] = b"\0\xffraw HTTP error body";

struct HttpFailureFixture {
    base_url: String,
    task: JoinHandle<()>,
}

impl HttpFailureFixture {
    async fn spawn() -> Result<Self> {
        let app = Router::new()
            .route(
                "/status/{status}",
                get(|Path(status): Path<u16>| async move {
                    (StatusCode::from_u16(status).unwrap(), Html(HTML))
                }),
            )
            .route(
                "/raw",
                get(|| async {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("content-type", "application/octet-stream")],
                        RAW_BODY,
                    )
                }),
            )
            .route(
                "/empty",
                get(|| async { Html("<!doctype html><body></body>") }),
            )
            .route(
                "/empty-error",
                get(|| async { (StatusCode::FORBIDDEN, Html("<!doctype html><body></body>")) }),
            )
            .route(
                "/redirect/{status}",
                get(|Path(status): Path<u16>| async move {
                    Redirect::temporary(&format!("/status/{status}"))
                }),
            )
            .route(
                "/challenge/{status}",
                get(|Path(status): Path<u16>| async move {
                    (
                        StatusCode::FORBIDDEN,
                        Html(format!(
                            "<!doctype html><script>location.replace('/status/{status}')</script>"
                        )),
                    )
                        .into_response()
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self { base_url, task })
    }

    fn run(&self, path: &str, args: &[&str]) -> Result<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_moli"))
            .args([
                "fetch",
                "--http-no-proxy",
                "*",
                "--timeout",
                "5000",
                "--redirect-wait-ms",
                "0",
            ])
            .args(args)
            .arg(format!("{}{path}", self.base_url))
            .output()?)
    }

    fn assert_http_failure(&self, output: &Output, status: u16, final_path: &str) {
        assert_eq!(output.status.code(), Some(22), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("HTTP status {status}")),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!("{}{final_path}", self.base_url)),
            "{stderr}"
        );
        assert_eq!(stderr.lines().count(), 2, "{stderr}");
    }
}

impl Drop for HttpFailureFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_http_errors_keep_selected_output_and_exit_zero() -> Result<()> {
    let server = HttpFailureFixture::spawn().await?;
    for status in [403, 500] {
        for dump in ["html", "markdown", "json"] {
            let output = server.run(&format!("/status/{status}"), &["--dump", dump])?;
            assert_eq!(output.status.code(), Some(0), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            match dump {
                "markdown" => assert_eq!(output.stdout, b"# HTTP fixture\n\nresponse body"),
                "json" => {
                    let value: Value = serde_json::from_slice(&output.stdout)?;
                    assert_eq!(value["status"], status);
                    assert!(value["html"].as_str().unwrap().contains("response body"));
                }
                _ => assert!(
                    String::from_utf8_lossy(&output.stdout).contains("<h1>HTTP fixture</h1>")
                ),
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fail_suppresses_output_and_exits_22_for_4xx_and_5xx() -> Result<()> {
    let server = HttpFailureFixture::spawn().await?;
    for status in [400, 403, 500, 599] {
        let path = format!("/status/{status}");
        for flag in ["--fail", "-f"] {
            let output = server.run(&path, &[flag, "--dump", "markdown"])?;
            server.assert_http_failure(&output, status, &path);
            assert!(output.stdout.is_empty(), "{output:?}");
        }
    }
    // Suppression also avoids evaluating an expression on the failed page.
    let output = server.run(
        "/status/403",
        &[
            "--fail",
            "--eval",
            "(() => { throw new Error('must not run'); })()",
        ],
    )?;
    server.assert_http_failure(&output, 403, "/status/403");
    assert!(output.stdout.is_empty(), "{output:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fail_with_body_preserves_selected_output_format() -> Result<()> {
    let server = HttpFailureFixture::spawn().await?;
    for args in [
        vec![],
        vec!["--dump", "html"],
        vec!["--dump", "markdown"],
        vec!["--dump", "json"],
        vec!["--dump", "semantic_tree"],
        vec!["--dump", "semantic_tree_text"],
        vec!["--eval", "document.querySelector('h1').textContent"],
    ] {
        let expected = server.run("/status/500", &args)?;
        assert_eq!(expected.status.code(), Some(0), "{expected:?}");
        assert!(!expected.stdout.is_empty(), "{args:?}");
        let mut fail_args = args.clone();
        fail_args.push("--fail-with-body");
        let output = server.run("/status/500", &fail_args)?;
        server.assert_http_failure(&output, 500, "/status/500");
        assert_eq!(output.stdout, expected.stdout, "{args:?}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_failure_modes_handle_raw_binary_and_json_output() -> Result<()> {
    let server = HttpFailureFixture::spawn().await?;
    for args in [vec![], vec!["--dump", "json"]] {
        let expected = server.run("/raw", &args)?;
        assert_eq!(expected.status.code(), Some(0), "{expected:?}");
        if args.is_empty() {
            assert_eq!(expected.stdout, RAW_BODY);
        } else {
            let value: Value = serde_json::from_slice(&expected.stdout)?;
            assert_eq!(value["status"], 503);
            assert_eq!(
                STANDARD.decode(value["body_base64"].as_str().unwrap())?,
                RAW_BODY
            );
        }
        for flag in ["--fail", "--fail-with-body"] {
            let mut fail_args = args.clone();
            fail_args.push(flag);
            let output = server.run("/raw", &fail_args)?;
            server.assert_http_failure(&output, 503, "/raw");
            if flag == "--fail" {
                assert!(output.stdout.is_empty(), "{output:?}");
            } else {
                assert_eq!(output.stdout, expected.stdout);
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_failure_modes_accept_success_3xx_and_empty_markdown() -> Result<()> {
    let server = HttpFailureFixture::spawn().await?;
    for flag in [None, Some("--fail"), Some("--fail-with-body")] {
        let mut args = vec!["--dump", "markdown"];
        args.extend(flag);
        let output = server.run("/empty-error", &args)?;
        assert!(output.stdout.is_empty(), "{output:?}");
        if flag.is_some() {
            server.assert_http_failure(&output, 403, "/empty-error");
        } else {
            assert_eq!(output.status.code(), Some(0), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
        }
    }
    for flag in ["--fail", "--fail-with-body"] {
        for path in ["/status/200", "/status/399", "/empty"] {
            let output = server.run(path, &[flag, "--dump", "markdown"])?;
            assert_eq!(output.status.code(), Some(0), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            if path == "/empty" {
                assert!(output.stdout.is_empty(), "{output:?}");
            } else {
                assert_eq!(output.stdout, b"# HTTP fixture\n\nresponse body");
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_failure_modes_preserve_binary_dump_formats() -> Result<()> {
    let server = HttpFailureFixture::spawn().await?;
    for dump in ["screenshot", "screenshot_full", "pdf"] {
        for flag in ["--fail", "--fail-with-body"] {
            let output = server.run("/status/403", &[flag, "--layout", "--dump", dump])?;
            server.assert_http_failure(&output, 403, "/status/403");
            if flag == "--fail" {
                assert!(output.stdout.is_empty(), "{dump}: {output:?}");
            } else {
                let signature: &[u8] = if dump == "pdf" {
                    b"%PDF-"
                } else {
                    b"\x89PNG\r\n\x1a\n"
                };
                assert!(output.stdout.starts_with(signature), "{dump}: {output:?}");
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_failure_modes_use_final_status_after_http_and_script_redirects() -> Result<()> {
    let server = HttpFailureFixture::spawn().await?;
    for flag in ["--fail", "--fail-with-body"] {
        for prefix in ["redirect", "challenge"] {
            for status in [200, 500] {
                let output = server.run(
                    &format!("/{prefix}/{status}"),
                    &[flag, "--dump", "json", "--wait-selector", "h1"],
                )?;
                if status == 200 {
                    assert_eq!(output.status.code(), Some(0), "{output:?}");
                    assert!(output.stderr.is_empty(), "{output:?}");
                } else {
                    server.assert_http_failure(&output, 500, "/status/500");
                    if flag == "--fail" {
                        assert!(output.stdout.is_empty(), "{output:?}");
                        continue;
                    }
                }
                let value: Value = serde_json::from_slice(&output.stdout)?;
                assert_eq!(value["status"], status);
                assert_eq!(
                    value["final_url"],
                    format!("{}/status/{status}", server.base_url)
                );
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_failure_modes_preserve_fetch_and_xhr_error_responses() -> Result<()> {
    let server = HttpFailureFixture::spawn().await?;
    let expression = r#"(async () => {
        const response = await fetch('/status/403');
        const text = await response.text();
        const xhr = await new Promise((resolve, reject) => {
            const request = new XMLHttpRequest();
            request.open('GET', '/status/500');
            request.onload = () => resolve({status: request.status, body: request.responseText.includes('response body')});
            request.onerror = reject;
            request.send();
        });
        return {status: response.status, ok: response.ok, body: text.includes('response body'), xhr};
    })()"#;
    for flag in ["--fail", "--fail-with-body"] {
        let output = server.run("/status/200", &[flag, "--eval", expression])?;
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            value,
            json!({
                "status": 403,
                "ok": false,
                "body": true,
                "xhr": {"status": 500, "body": true}
            })
        );
    }
    Ok(())
}

#[test]
fn http_failure_modes_keep_exit_code_one_for_non_http_errors() -> Result<()> {
    for flag in ["--fail", "--fail-with-body"] {
        let output = Command::new(env!("CARGO_BIN_EXE_moli"))
            .args(["fetch", flag, "https://["])
            .output()?;
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(!output.stderr.is_empty(), "{output:?}");
    }
    Ok(())
}
