use axum::{
    http::{header, StatusCode},
    response::IntoResponse,
};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "static/"]
struct Assets;

pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'";

/// Serve an embedded static file.
pub async fn serve_static(path: &str) -> impl IntoResponse {
    let path = if path.is_empty() || path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };

    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        CONTENT_SECURITY_POLICY.to_string(),
                    ),
                ],
                content.data.to_vec(),
            )
                .into_response()
        }
        None => {
            // SPA fallback — serve index.html for client-side routing
            match Assets::get("index.html") {
                Some(content) => (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "text/html".to_string()),
                        (
                            header::CONTENT_SECURITY_POLICY,
                            CONTENT_SECURITY_POLICY.to_string(),
                        ),
                    ],
                    content.data.to_vec(),
                )
                    .into_response(),
                None => (StatusCode::NOT_FOUND, "Not found").into_response(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_responses_carry_csp_header() {
        for path in ["index.html", "no-such-route"] {
            let response = serve_static(path).await.into_response();
            let csp = response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .expect("CSP header present")
                .to_str()
                .unwrap()
                .to_string();
            assert!(csp.contains("object-src 'none'"));
            assert!(csp.contains("frame-ancestors 'none'"));
            assert!(!csp.contains("http:") && !csp.contains("https:"));
            assert!(
                !csp.contains("script-src 'self' 'unsafe-inline'"),
                "inline scripts must stay banned: {csp}"
            );
            assert!(
                !csp.contains("style-src 'self' 'unsafe-inline'"),
                "inline styles must stay banned: {csp}"
            );
        }
    }

    #[test]
    fn embedded_page_has_no_inline_scripts_or_handlers() {
        let html = Assets::get("index.html").expect("index.html embedded");
        let text = std::str::from_utf8(&html.data).expect("html is utf-8");
        assert!(
            !text.contains("onclick="),
            "inline handlers violate script-src"
        );
        assert!(
            !text.contains("<script>"),
            "inline script blocks violate script-src"
        );
        assert!(
            text.contains(r#"<script src="/app.js">"#),
            "page must load the external script"
        );
        let js = Assets::get("app.js").expect("app.js embedded");
        let js_text = std::str::from_utf8(&js.data).expect("js is utf-8");
        assert!(
            !js_text.contains("onclick="),
            "generated markup must not use inline handlers"
        );
        assert!(
            js_text.contains("data-action"),
            "click handling must go through delegation"
        );
    }

    #[test]
    fn embedded_page_has_no_inline_styles() {
        let html = Assets::get("index.html").expect("index.html embedded");
        let text = std::str::from_utf8(&html.data).expect("html is utf-8");
        assert!(!text.contains("<style"), "style blocks violate style-src");
        assert!(
            !text.contains("style=\""),
            "inline style attributes violate style-src"
        );
        assert!(
            text.contains(r#"href="/app.css""#),
            "page must load the external stylesheet"
        );
        let css = Assets::get("app.css").expect("app.css embedded");
        let css_text = std::str::from_utf8(&css.data).expect("css is utf-8");
        assert!(
            css_text.contains(".progress-bar-fill"),
            "stylesheet must carry the app styles"
        );
        assert!(
            css_text.contains(".mono-12"),
            "stylesheet must carry the utility classes"
        );
        let js = Assets::get("app.js").expect("app.js embedded");
        let js_text = std::str::from_utf8(&js.data).expect("js is utf-8");
        assert!(
            !js_text.contains("style=\""),
            "generated markup must not use inline styles"
        );
    }
}
