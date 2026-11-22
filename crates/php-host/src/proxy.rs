//! One loopback HTTP call from Rust into the PHP host. Plugin routes are
//! runtime data the PHP loader decides, so the app exposes one generic
//! `php_request(method, path, body)` rather than a command per route.

use serde::Serialize;
use std::time::Duration;

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PhpResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

/// Forward `method path` with an optional JSON body to the host on `port`.
/// `path` must start with `/`; a path that tries to leave the host (a scheme,
/// a host name) is refused before any request is made.
pub async fn forward(
    port: u16,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<PhpResponse, String> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(format!("invalid php-host path: {path}"));
    }
    let http_method: reqwest::Method = method
        .parse()
        .map_err(|_| format!("invalid HTTP method: {method}"))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|e| format!("failed to build the http client: {e}"))?;
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut request = client.request(http_method, &url);
    if let Some(json) = &body {
        request = request.json(json);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("php-host request failed: {e}"))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read the php-host response: {e}"))?;
    let body = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
    };
    Ok(PhpResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_path_that_is_not_local_is_refused() {
        assert!(forward(1, "GET", "http://example.com/", None)
            .await
            .is_err());
        assert!(forward(1, "GET", "//example.com/x", None).await.is_err());
        assert!(forward(1, "GET", "api/x", None).await.is_err());
    }

    #[tokio::test]
    async fn the_answer_carries_status_and_parsed_json() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(&mut request.as_reader(), &mut body);
                let answer = format!(
                    "{{\"echo\":{},\"path\":\"{}\"}}",
                    if body.is_empty() { "null".into() } else { body },
                    request.url()
                );
                let _ =
                    request.respond(tiny_http::Response::from_string(answer).with_status_code(201));
            }
        });
        let response = forward(port, "POST", "/api/x", Some(serde_json::json!({"a": 1})))
            .await
            .unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.body["echo"]["a"], 1);
        assert_eq!(response.body["path"], "/api/x");
    }
}
