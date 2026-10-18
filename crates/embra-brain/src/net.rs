//! The reqwest clients of the brain, built one way.
//!
//! Every client the brain talks to a provider or a git host with follows a
//! redirect to the same origin only ([`same_origin_redirect_policy`]).
//! reqwest's default follows up to ten hops and strips `Authorization`,
//! `Cookie` and `Proxy-Authorization` on a cross-host hop — and nothing
//! else, so `PRIVATE-TOKEN`, `x-api-key` and `x-goog-api-key` would travel
//! to a third host. A same-origin redirect (an endpoint behind a proxy)
//! still works; a cross-origin one comes back as the 3xx it is, and the
//! caller reports it. The database client follows no redirect at all: a
//! loopback API that redirects is a fault.

use reqwest::redirect::{Attempt, Policy};

/// The most same-origin hops a request follows, reqwest's own default.
const MAX_REDIRECTS: usize = 10;

/// A redirect is followed when its target has the scheme, host and port of
/// the URL that redirected, and the chain is under [`MAX_REDIRECTS`].
pub(crate) fn same_origin_redirect_policy() -> Policy {
    Policy::custom(|attempt: Attempt<'_>| {
        let from = attempt.previous().last();
        let to = attempt.url();
        let same_origin = from.is_some_and(|f| {
            f.scheme() == to.scheme()
                && f.host_str() == to.host_str()
                && f.port_or_known_default() == to.port_or_known_default()
        });
        if same_origin && attempt.previous().len() <= MAX_REDIRECTS {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// A client builder with the redirect policy set; the caller adds its
/// timeouts and roots.
pub(crate) fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().redirect(same_origin_redirect_policy())
}

/// A client with the redirect policy and nothing else.
pub(crate) fn client() -> reqwest::Client {
    builder()
        .build()
        .expect("a reqwest client with a redirect policy and no other option builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_cross_origin_redirect_is_returned_and_a_same_origin_one_is_followed() {
        let home = MockServer::start().await;
        let away = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/leave"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/landed", away.uri())))
            .mount(&home)
            .await;
        Mock::given(method("GET"))
            .and(path("/stay"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/landed"))
            .mount(&home)
            .await;
        Mock::given(method("GET"))
            .and(path("/landed"))
            .respond_with(ResponseTemplate::new(200).set_body_string("home"))
            .mount(&home)
            .await;
        Mock::given(method("GET"))
            .and(path("/landed"))
            .respond_with(ResponseTemplate::new(200).set_body_string("away"))
            .mount(&away)
            .await;

        let c = client();
        let r = c
            .get(format!("{}/leave", home.uri()))
            .header("x-api-key", "k-0123456789")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302, "the cross-origin redirect is handed back");
        assert!(away.received_requests().await.unwrap().is_empty(), "the other origin saw nothing");

        let r = c.get(format!("{}/stay", home.uri())).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "home");
    }

    #[tokio::test]
    async fn the_database_client_follows_no_redirect() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/e1"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/memory.entries/docs/e2"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/e2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": {"_id": "e2"}, "meta": {}})))
            .mount(&server)
            .await;
        let db = crate::db::WardsonDbClient::from_url(&server.uri());
        let err = db.read("memory.entries", "e1").await.unwrap_err().to_string();
        assert!(err.contains("302"), "the redirect is an error, not a document: {err}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "one request, no hop");
    }
}
