//! The `/v2` routes and request authentication.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Extension, Json, Router};
use magnetite_sync_proto::{
    BodyDigest, CONTENT_DIGEST, CreateChainRequest, ErrorResponse, PutCollectionRequest,
    PutCollectionResponse, Sealed, SignedHeader, is_collection_id,
};
use subtle::ConstantTimeEq;

use crate::db::{Create, Put};
use crate::gate::{self, Gate};
use crate::{AppState, Registration};

/// Largest request body (a padded 16 MiB collection in base64 plus JSON).
const MAX_BODY: usize = 24 * 1024 * 1024;
/// Largest body of the requests that carry no collection (`POST /chain`).
const MAX_SMALL_BODY: usize = 4 * 1024;
const MAX_CIPHERTEXT: usize = 22 * 1024 * 1024;

/// Chains with a collection write in progress in this process: one at a
/// time per chain, so one chain's slow uploads cannot take every slot.
#[derive(Default)]
struct Uploads(Mutex<HashSet<String>>);

/// A write in progress; it ends when dropped.
struct Upload<'a> {
    uploads: &'a Uploads,
    chain: String,
}

impl Uploads {
    fn begin(&self, chain: &str) -> Option<Upload<'_>> {
        let mut chains = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        chains.insert(chain.to_owned()).then(|| Upload {
            uploads: self,
            chain: chain.to_owned(),
        })
    }
}

impl Drop for Upload<'_> {
    fn drop(&mut self) {
        self.uploads
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.chain);
    }
}

/// What `authorize` works with.
#[derive(Clone)]
struct Auth {
    app: Arc<AppState>,
    uploads: Arc<Uploads>,
}

/// `/health`, `/ready`, and the signed routes behind the gate and
/// authentication.
pub fn routes(state: AppState) -> Router {
    let shared = Arc::new(state);
    let auth = Auth {
        app: shared.clone(),
        uploads: Arc::default(),
    };
    let signed = Router::new()
        .route(
            "/chain",
            get(chain_exists).post(create_chain).delete(delete_chain),
        )
        .route("/collections/{id}", put(put_collection).get(get_collection))
        // Outermost first: verify the signature, take a permit, then the
        // database work.
        .layer(middleware::from_fn_with_state(auth, authorize))
        .layer(middleware::from_fn_with_state(
            Arc::new(Gate::new()),
            gate::admit,
        ))
        .layer(middleware::from_fn(verify))
        .with_state(shared.clone());
    // Unsigned and outside the gate: `/health` (liveness) never touches the
    // database, `/ready` (readiness) asks it at most once a second.
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ready", get(ready))
        .with_state(shared)
        .merge(signed)
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

pub struct ApiError(StatusCode, ErrorResponse);

impl ApiError {
    pub(crate) fn new(status: StatusCode, msg: &str) -> Self {
        Self(
            status,
            ErrorResponse {
                error: msg.into(),
                current_version: None,
            },
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // A lock or a statement that took too long (`db::SESSION`), or no
        // free connection: busy, not broken.
        let busy = e.downcast_ref::<sqlx::Error>().is_some_and(|e| match e {
            sqlx::Error::PoolTimedOut => true,
            sqlx::Error::Database(d) => matches!(d.code().as_deref(), Some("55P03" | "57014")),
            _ => false,
        });
        if busy {
            return Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the server is busy, try again in a moment",
            );
        }
        tracing::error!(error = %e, "internal error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

/// The sync chain that signed the request (hex public key).
#[derive(Clone)]
pub struct Chain {
    pub id: String,
    /// The chain exists on this server.
    pub known: bool,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

/// The answer to a bad signature.
const REFUSED: &str = "bad signature";

fn no_chain() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "no such sync chain")
}

/// What `verify` learned: the chain that signed, and for a collection write
/// the digest its body (read later, under a permit) must match.
#[derive(Clone)]
struct Verified {
    id: String,
    digest: Option<BodyDigest>,
}

fn too_large<E>(_: E) -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request too large")
}

/// Whether a request announces a body (HTTP/1.1 needs one of these headers).
fn has_body(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(header::CONTENT_LENGTH)
        .is_some_and(|v| v.as_bytes() != b"0")
        || headers.contains_key(header::TRANSFER_ENCODING)
}

/// First, before admission, so a request without a valid signature holds no
/// permit: the signature over method, host, path (below `/v2`), time and
/// body. A collection write is checked against its `Content-Digest`; its
/// body comes later (`authorize`). Any other request reads its body of at
/// most 4 KiB first, within `SMALL_TIMEOUT`; GET and DELETE carry none.
pub(crate) async fn verify(req: Request, next: Next) -> Result<Response, ApiError> {
    let (mut parts, body) = req.into_parts();
    let unauthorized = |msg: &str| ApiError::new(StatusCode::UNAUTHORIZED, msg);
    let header = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let signed = SignedHeader::parse(header, now_ms()).map_err(|e| unauthorized(&e.to_string()))?;
    // The host the client addressed (HTTP/1.1 `Host`, or the HTTP/2
    // authority) is signed: a request captured on one server is refused
    // by any other. Reverse proxies must pass the original `Host` on.
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            parts
                .uri
                .authority()
                .map(axum::http::uri::Authority::as_str)
        })
        .unwrap_or("");
    let (digest, body) = if gate::collection_write(&parts.method, parts.uri.path()) {
        let digest = parts
            .headers
            .get(CONTENT_DIGEST)
            .and_then(|v| v.to_str().ok())
            .and_then(BodyDigest::parse)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "a collection write needs a Content-Digest header",
                )
            })?;
        signed
            .verify_digest(parts.method.as_str(), host, parts.uri.path(), &digest)
            .map_err(|_| unauthorized(REFUSED))?;
        (Some(digest), body)
    } else {
        if matches!(parts.method, Method::GET | Method::DELETE) && has_body(&parts.headers) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "this request takes no body",
            ));
        }
        let bytes: Bytes =
            tokio::time::timeout(gate::SMALL_TIMEOUT, to_bytes(body, MAX_SMALL_BODY))
                .await
                .map_err(|_| ApiError::new(StatusCode::REQUEST_TIMEOUT, "request took too long"))?
                .map_err(too_large)?;
        signed
            .verify(parts.method.as_str(), host, parts.uri.path(), &bytes)
            .map_err(|_| unauthorized(REFUSED))?;
        (None, Body::from(bytes))
    };
    parts.extensions.insert(Verified {
        id: signed.account_id(),
        digest,
    });
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// Then, under a permit: what needs the database, and a collection write's
/// body. Only the key holder gets here, so only it learns whether the chain
/// exists (404 if not, except for `POST /chain`, which creates it).
async fn authorize(
    State(Auth { app, uploads }): State<Auth>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let (mut parts, body) = req.into_parts();
    let Verified { id, digest } = parts
        .extensions
        .remove::<Verified>()
        .ok_or_else(|| anyhow::anyhow!("a request reached authorize without verify"))?;
    // The upload in progress is held until the write is answered.
    let (body, _upload) = match digest {
        Some(digest) => {
            // A deleted chain does not exist, and cannot be started again
            // while its tombstone lasts (`Db::create_chain`).
            if !app.db.chain_exists(&id).await? {
                return Err(no_chain());
            }
            let upload = uploads.begin(&id).ok_or_else(|| {
                ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "another write of this sync chain is under way, try again in a moment",
                )
            })?;
            let bytes: Bytes = to_bytes(body, MAX_BODY).await.map_err(too_large)?;
            if !digest.matches(&bytes) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "the body does not match its Content-Digest",
                ));
            }
            (Body::from(bytes), Some(upload))
        }
        None => (body, None),
    };
    let creating = parts.method == Method::POST && parts.uri.path() == "/chain";
    let known = app.db.seen(&id).await?;
    if !known && !creating {
        return Err(no_chain());
    }
    parts.extensions.insert(Chain { id, known });
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// 200 while the database answers, 503 when it does not.
async fn ready(State(app): State<Arc<AppState>>) -> StatusCode {
    if app.db.ready().await {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn chain_exists() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn create_chain(
    State(app): State<Arc<AppState>>,
    Extension(chain): Extension<Chain>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    if chain.known {
        return Ok(StatusCode::NO_CONTENT);
    }
    let req: CreateChainRequest = if body.is_empty() {
        CreateChainRequest::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "bad request"))?
    };
    match &app.registration {
        Registration::Open => {}
        Registration::Closed => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "this server accepts no new sync chains",
            ));
        }
        Registration::Invite(code) => {
            let ok = req
                .invite
                .as_deref()
                .is_some_and(|i| bool::from(i.as_bytes().ct_eq(code.as_bytes())));
            if !ok {
                return Err(ApiError::new(StatusCode::FORBIDDEN, "invite code required"));
            }
        }
    }
    match app.db.create_chain(&chain.id, &app.limits).await? {
        Create::Created => Ok(StatusCode::CREATED),
        Create::Exists => Ok(StatusCode::NO_CONTENT),
        Create::Deleted => Err(ApiError::new(
            StatusCode::GONE,
            "this sync chain was deleted a moment ago",
        )),
        Create::Full => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this server has no room for new sync chains",
        )),
        Create::DailyLimit => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "this server accepts no more new sync chains today, try again tomorrow",
        )),
    }
}

async fn delete_chain(
    State(app): State<Arc<AppState>>,
    Extension(chain): Extension<Chain>,
) -> Result<StatusCode, ApiError> {
    app.db.delete_chain(&chain.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn check_id(id: &str) -> Result<(), ApiError> {
    if is_collection_id(id) {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::BAD_REQUEST, "bad collection id"))
    }
}

async fn get_collection(
    State(app): State<Arc<AppState>>,
    Extension(chain): Extension<Chain>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    check_id(&id)?;
    match app.db.collection(&chain.id, &id).await? {
        // Written into a buffer of about the right size: a growing one
        // would take up to twice a collection's ~21 MiB.
        Some(c) => {
            let mut body = Vec::with_capacity(c.nonce.len() + c.ciphertext.len() + 64);
            serde_json::to_writer(&mut body, &c).map_err(anyhow::Error::from)?;
            Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
        }
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "no such collection")),
    }
}

async fn put_collection(
    State(app): State<Arc<AppState>>,
    Extension(chain): Extension<Chain>,
    Path(id): Path<String>,
    Json(req): Json<PutCollectionRequest>,
) -> Result<Json<PutCollectionResponse>, ApiError> {
    check_id(&id)?;
    let base_version = req.base_version;
    let sealed = Sealed {
        nonce: req.nonce,
        ciphertext: req.ciphertext,
    };
    if sealed.ciphertext.len() > MAX_CIPHERTEXT {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "collection too large",
        ));
    }
    if !sealed.is_well_formed(MAX_CIPHERTEXT) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "malformed nonce or ciphertext",
        ));
    }
    match app
        .db
        .put_collection(&chain.id, &id, base_version, &sealed, &app.limits)
        .await?
    {
        Put::Stored(version) => Ok(Json(PutCollectionResponse { version })),
        Put::Conflict(current) => Err(ApiError(
            StatusCode::CONFLICT,
            ErrorResponse {
                error: "version conflict".into(),
                current_version: Some(current),
            },
        )),
        Put::TooManyCollections => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "too many collections in this sync chain",
        )),
        Put::OverQuota => Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "this sync chain has used up its storage on the server",
        )),
        Put::TooManyWrites => Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many writes for this sync chain, try again in a minute",
        )),
        Put::UnknownChain => Err(no_chain()),
    }
}
