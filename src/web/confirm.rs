//! The confirm page: what the plugin is about to change in Studio, shown in the browser.
//!
//! On connect, the plugin used to build a tree of every pending change inside Studio before
//! asking to accept them, which is slow for big projects and cramped in a dock widget. Instead
//! it posts a plain description of the changes here, and the server opens a page that shows them
//! with line diffs for scripts. Accept and Abort work from either the page or the plugin: both
//! post the decision here, and each side polls for the other's.

use std::net::SocketAddr;
use std::sync::Mutex;

use hyper::{body, header, Body, Method, Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::web::origin::canonical;

/// Where the page is served. The plugin reads it from `/api/rojo` to know the server has one.
pub const PAGE: &str = "/confirm/";

static PAGE_HTML: &str = include_str!("confirm.html");

struct Pending {
    id: u64,
    report: Value,
    decision: Option<String>,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static NEXT_ID: Mutex<u64> = Mutex::new(1);

/// True for a request this module answers.
pub fn handles(path: &str) -> bool {
    path == "/confirm"
        || path == PAGE
        || path == "/api/confirm"
        || path.starts_with("/api/confirm/")
}

pub async fn call(remote_addr: SocketAddr, request: Request<Body>) -> Response<Body> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    // Only the plugin on this machine may make the server open a browser.
    let local = canonical(remote_addr.ip()).is_loopback();

    match (&method, path.as_str()) {
        (&Method::GET, "/confirm") => Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(header::LOCATION, PAGE)
            .body(Body::empty())
            .unwrap(),
        (&Method::GET, PAGE) => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(PAGE_HTML))
            .unwrap(),

        // The plugin posts the changes it's about to make.
        (&Method::POST, "/api/confirm") => {
            let report: Value = match read_json(request).await {
                Ok(value) => value,
                Err(response) => return response,
            };
            let id = {
                let mut next = NEXT_ID.lock().unwrap();
                let id = *next;
                *next += 1;
                id
            };
            *PENDING.lock().unwrap() = Some(Pending {
                id,
                report,
                decision: None,
            });
            if local {
                open_page(&host);
            }
            reply(StatusCode::OK, json!({ "id": id }))
        }

        // The page reads the changes, and later whether they were decided.
        (&Method::GET, "/api/confirm") => match &*PENDING.lock().unwrap() {
            Some(pending) => reply(
                StatusCode::OK,
                json!({ "id": pending.id, "report": pending.report, "decision": pending.decision }),
            ),
            None => reply(StatusCode::OK, json!({ "id": null })),
        },

        // The plugin polls for a decision made on the page.
        (&Method::GET, "/api/confirm/decision") => {
            let id = query_id(&query);
            match &*PENDING.lock().unwrap() {
                Some(pending) if Some(pending.id) == id => {
                    reply(StatusCode::OK, json!({ "decision": pending.decision }))
                }
                _ => reply(
                    StatusCode::GONE,
                    json!({ "error": "not the pending changes" }),
                ),
            }
        }

        // Either side decides.
        (&Method::POST, "/api/confirm/decide") => {
            #[derive(Deserialize)]
            struct Decide {
                id: u64,
                decision: String,
            }
            let decide: Decide = match read_json(request).await {
                Ok(value) => value,
                Err(response) => return response,
            };
            if decide.decision != "Accept" && decide.decision != "Abort" {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "the decision is Accept or Abort" }),
                );
            }
            match &mut *PENDING.lock().unwrap() {
                Some(pending) if pending.id == decide.id => {
                    // The first decision stands.
                    if pending.decision.is_none() {
                        pending.decision = Some(decide.decision);
                    }
                    reply(StatusCode::OK, json!({ "decision": pending.decision }))
                }
                _ => reply(
                    StatusCode::GONE,
                    json!({ "error": "not the pending changes" }),
                ),
            }
        }

        // The plugin's "View diff" button, since plugins can't open a browser themselves.
        (&Method::POST, "/api/confirm/open") => {
            if local {
                open_page(&host);
            }
            reply(StatusCode::OK, json!({}))
        }

        _ => reply(
            StatusCode::NOT_FOUND,
            json!({ "error": format!("no route {method} {path}") }),
        ),
    }
}

fn open_page(host: &str) {
    let url = format!("http://{host}{PAGE}");
    if let Err(error) = opener::open(&url) {
        log::warn!("Couldn't open {url} in the browser: {error}");
    }
}

fn query_id(query: &str) -> Option<u64> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("id="))
        .and_then(|value| value.parse().ok())
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    request: Request<Body>,
) -> Result<T, Response<Body>> {
    let bytes = body::to_bytes(request.into_body()).await.map_err(|error| {
        reply(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("couldn't read the request: {error}") }),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        reply(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("the request isn't valid JSON: {error}") }),
        )
    })
}

fn reply(status: StatusCode, value: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn only_confirm_paths_are_routed_here() {
        assert!(handles("/confirm"));
        assert!(handles("/confirm/"));
        assert!(handles("/api/confirm"));
        assert!(handles("/api/confirm/decide"));
        assert!(!handles("/api/confirmx"));
        assert!(!handles("/api/rojo"));
    }

    #[test]
    fn the_id_is_read_from_the_query() {
        assert_eq!(query_id("id=12"), Some(12));
        assert_eq!(query_id("x=1&id=3"), Some(3));
        assert_eq!(query_id("id=abc"), None);
        assert_eq!(query_id(""), None);
    }
}
