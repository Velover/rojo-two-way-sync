//! Hands requests under `/twoway` to the twoway sync, and turns its replies into responses.

use std::sync::Arc;

use hyper::{header, Body, Request, Response, StatusCode};
use twoway::service::{Reply, Request as TwowayRequest, Service};

use crate::twoway_sync::BASE;

pub async fn call(service: Arc<Service>, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let rest = &parts.uri.path()[BASE.len()..];

    // The review page uses relative URLs, which need the trailing slash.
    if rest.is_empty() {
        return Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(header::LOCATION, format!("{BASE}/"))
            .body(Body::empty())
            .unwrap();
    }

    let body = match hyper::body::to_bytes(body).await {
        Ok(bytes) => bytes.to_vec(),
        Err(error) => {
            return text(
                StatusCode::BAD_REQUEST,
                format!("could not read the request: {error}"),
            )
        }
    };
    let request = TwowayRequest {
        method: parts.method.as_str().to_string(),
        path: rest.to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        body,
    };

    match service.handle(request).await {
        Reply::Json(status, value) => Response::builder()
            .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap_or_default()))
            .unwrap(),
        Reply::Bytes(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(bytes))
            .unwrap(),
        Reply::Html(page) => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(page))
            .unwrap(),
    }
}

fn text(status: StatusCode, message: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(message))
        .unwrap()
}
