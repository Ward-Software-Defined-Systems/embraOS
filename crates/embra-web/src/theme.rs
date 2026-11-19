//! `POST /api/theme`: the theme picker's door.
//!
//! One theme is in force for the whole machine (`embra_tools_core::theme`):
//! its name is one small file on STATE. This service reads the file for
//! every page it serves (`assets::static_handler` writes the name onto the
//! page's root) and for every status answer, and this route writes it. The
//! brain's `/theme` writes the same file. The picker comes here and not to
//! the console because it must work where typing does not: at a selector of
//! the first-boot wizard, from a browser that only observes, while a turn
//! runs.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use embra_tools_core::theme;
use serde_json::{Value, json};

use crate::state::AppState;

fn refusal(status: StatusCode, said: String) -> Response {
    (status, Json(json!({ "error": said }))).into_response()
}

fn names() -> String {
    theme::THEMES.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
}

/// Store the theme the body names: `{"theme": "<name>"}`.
pub async fn api_theme_set(State(st): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(name) = body.get("theme").and_then(Value::as_str).map(str::to_string) else {
        return refusal(StatusCode::BAD_REQUEST, "the body names no theme: {\"theme\": \"<name>\"}".to_string());
    };
    let file = st.theme_file.clone();
    // A write, a sync and a rename: off the async threads.
    let written = tokio::task::spawn_blocking(move || theme::write(&file, &name)).await;
    match written {
        Ok(Ok(theme)) => Json(json!({ "theme": theme.name })).into_response(),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidInput => {
            refusal(StatusCode::BAD_REQUEST, format!("{e}; the themes are {}", names()))
        }
        Ok(Err(e)) => refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("the theme could not be stored: {e}")),
        Err(e) => refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("the theme could not be stored: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn answer(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn a_theme_of_the_list_is_stored_and_the_status_and_the_page_name_it() {
        let (st, _input) = AppState::stub();
        let other = theme::THEMES.last().expect("a theme");
        assert_ne!(other.name, theme::DEFAULT, "the list has more than the default");

        let (status, said) = answer(api_theme_set(State(st.clone()), Json(json!({ "theme": other.name }))).await).await;
        assert_eq!((status, &said), (StatusCode::OK, &json!({ "theme": other.name })));
        assert_eq!(std::fs::read_to_string(&*st.theme_file).unwrap(), format!("{}\n", other.name));

        let Json(status) = crate::status::api_status(State(st.clone())).await;
        assert_eq!(status["theme"], other.name);
        let page = crate::assets::static_handler(State(st), axum::http::Uri::from_static("/embraOS")).await;
        let page = axum::body::to_bytes(page.into_body(), usize::MAX).await.unwrap();
        let page = std::str::from_utf8(&page).unwrap();
        assert!(page.contains(&format!("<html data-theme=\"{}\"", other.name)), "{}", &page[..page.len().min(200)]);
    }

    #[tokio::test]
    async fn a_machine_that_has_stored_no_theme_is_on_the_default() {
        let (st, _input) = AppState::stub();
        assert!(!st.theme_file.exists());
        let Json(status) = crate::status::api_status(State(st.clone())).await;
        assert_eq!(status["theme"], theme::DEFAULT);
        let page = crate::assets::static_handler(State(st), axum::http::Uri::from_static("/embraOS")).await;
        let page = axum::body::to_bytes(page.into_body(), usize::MAX).await.unwrap();
        assert!(std::str::from_utf8(&page).unwrap().contains(&format!("<html data-theme=\"{}\"", theme::DEFAULT)));
    }

    #[tokio::test]
    async fn a_name_that_is_no_theme_is_refused_with_the_list_and_nothing_is_written() {
        let (st, _input) = AppState::stub();
        for body in [json!({ "theme": "plaid" }), json!({ "theme": "../ember" }), json!({ "theme": "" })] {
            let (status, said) = answer(api_theme_set(State(st.clone()), Json(body.clone())).await).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let said = said["error"].as_str().expect("an error");
            assert!(said.starts_with("no theme is called ") && said.contains("; the themes are ember, "), "{said}");
        }
        for body in [json!({}), json!({ "theme": 7 }), json!({ "name": "ember" })] {
            let (status, said) = answer(api_theme_set(State(st.clone()), Json(body.clone())).await).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(said["error"].as_str().unwrap().starts_with("the body names no theme"), "{said}");
        }
        assert!(!st.theme_file.exists());
    }

    #[tokio::test]
    async fn a_theme_that_cannot_be_stored_is_said_and_is_no_success() {
        let (mut st, _input) = AppState::stub();
        st.theme_file = std::sync::Arc::from(st.theme_file.parent().unwrap().join("no-such-directory").join("theme").as_path());
        let (status, said) = answer(api_theme_set(State(st), Json(json!({ "theme": theme::DEFAULT }))).await).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(said["error"].as_str().unwrap().starts_with("the theme could not be stored: "), "{said}");
    }
}
