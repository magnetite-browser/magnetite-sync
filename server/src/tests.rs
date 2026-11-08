//! Tests against Postgres: `#[sqlx::test]` gives each one a fresh database on
//! the server `DATABASE_URL` names (CONTRIBUTING.md).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::routing::{post, put};
use http_body_util::BodyExt;
use magnetite_sync_proto::{CONTENT_DIGEST, ChainKeys, SyncCode, content_digest};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection as _, PgPool};
use tower::ServiceExt;

use crate::db::{CHAINS_LOCK, Create, TOMBSTONE_TTL, today};
use crate::gate::{self, Gate, SMALL_TIMEOUT, WRITE_TIMEOUT};
use crate::{AppState, Db, Limits, Purged, Registration, maintain, router};

/// The server name tests sign for and send as `Host`.
const HOST: &str = "sync.test";

fn app(pool: &PgPool, reg: Registration) -> Router {
    app_with(pool, reg, Limits::default())
}

/// One server on `pool`; several of them stand for several server processes
/// sharing a database.
fn app_with(pool: &PgPool, reg: Registration, limits: Limits) -> Router {
    router(AppState {
        db: Db::from_pool(pool.clone()),
        registration: reg,
        retention_days: Some(400),
        limits,
    })
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A request for `path` (below /v2) with an optional `Authorization` header.
fn request(method: &str, path: &str, authorization: Option<&str>, body: Body) -> Request<Body> {
    let mut req = Request::builder()
        .header("host", HOST)
        .method(method)
        .uri(format!("/v2{path}"))
        .header("content-type", "application/json");
    if let Some(a) = authorization {
        req = req.header("authorization", a);
    }
    req.body(body).unwrap()
}

/// `req` with a `Content-Digest` of `of`, as clients send it.
fn digested(mut req: Request<Body>, of: &[u8]) -> Request<Body> {
    req.headers_mut()
        .insert(CONTENT_DIGEST, content_digest(of).parse().unwrap());
    req
}

/// A request signed now by `keys` (unsigned without), with its digest.
fn signed(
    keys: Option<&ChainKeys>,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
    let auth = keys.map(|k| k.authorization(method, HOST, path, &bytes, now_ms()));
    let req = request(method, path, auth.as_deref(), Body::from(bytes.clone()));
    digested(req, &bytes)
}

async fn call(
    app: &Router,
    keys: Option<&ChainKeys>,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    send(app, signed(keys, method, path, body)).await
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let out = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&out).unwrap_or(serde_json::Value::Null),
    )
}

/// The body of a write of `plain` as version `base + 1` of collection `name`.
fn write_body(keys: &ChainKeys, name: &str, base: u64, plain: &[u8]) -> serde_json::Value {
    let sealed = keys
        .seal(&keys.collection_id(name), base + 1, plain)
        .unwrap();
    serde_json::json!({"base_version": base, "nonce": sealed.nonce, "ciphertext": sealed.ciphertext})
}

/// A body that never arrives while the sender lives.
fn stalled() -> (http_body_util::channel::Sender<Bytes>, Body) {
    let (sender, body) = http_body_util::Channel::<Bytes, std::convert::Infallible>::new(1);
    (sender, Body::new(body))
}

/// Run `sql` on the test database (moving stored times into the past).
async fn exec(pool: &PgPool, sql: &'static str) {
    sqlx::query(sql).execute(pool).await.unwrap();
}

#[sqlx::test]
async fn chain_lifecycle_with_signed_requests(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let code = SyncCode::generate();
    let keys = code.keys();
    let id = keys.collection_id("sidebar");
    let path = format!("/collections/{id}");

    // Unknown chains are not found until created; unsigned requests are refused.
    assert_eq!(
        call(&app, Some(&keys), "GET", "/chain", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, None, "GET", "/chain", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, Some(&keys), "POST", "/chain", None).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        call(&app, Some(&keys), "GET", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );

    // Write, conflict, read back on a second device with the same code.
    let body = write_body(&keys, "sidebar", 0, b"{\"spaces\":[]}");
    let (st, out) = call(&app, Some(&keys), "PUT", &path, Some(body.clone())).await;
    assert_eq!((st, out["version"].as_u64()), (StatusCode::OK, Some(1)));
    let (st, out) = call(&app, Some(&keys), "PUT", &path, Some(body)).await;
    assert_eq!(
        (st, out["current_version"].as_u64()),
        (StatusCode::CONFLICT, Some(1)),
        "replayed or stale write"
    );
    let other_device = code.keys();
    let (st, out) = call(&app, Some(&other_device), "GET", &path, None).await;
    assert_eq!(st, StatusCode::OK);
    let got: magnetite_sync_proto::CollectionResponse = serde_json::from_value(out).unwrap();
    assert_eq!(
        other_device.open(&id, got.version, &got.sealed()).unwrap(),
        b"{\"spaces\":[]}"
    );

    // Another chain cannot read it (it is not even known), bad ids are refused.
    let stranger = SyncCode::generate().keys();
    assert_eq!(
        call(&app, Some(&stranger), "GET", &path, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, Some(&keys), "GET", "/collections/sidebar", None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    // A signature for one request does not authorise another.
    let header = keys.authorization("GET", HOST, &path, b"", now_ms());
    assert_eq!(
        send(
            &app,
            request("DELETE", "/chain", Some(&header), Body::empty())
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    // Erase everything.
    assert_eq!(
        call(&app, Some(&keys), "DELETE", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, Some(&keys), "GET", &path, None).await.0,
        StatusCode::NOT_FOUND
    );
    let left: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM chains), (SELECT count(*) FROM collections)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, (0, 0), "nothing of the chain is left");
}

#[sqlx::test]
async fn only_the_key_holder_learns_that_a_chain_does_not_exist(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = SyncCode::generate().keys();
    let path = format!("/collections/{}", keys.collection_id("sidebar"));
    for (method, path) in [
        ("GET", "/chain"),
        ("DELETE", "/chain"),
        ("GET", &path),
        ("PUT", &path),
    ] {
        assert_eq!(
            call(&app, Some(&keys), method, path, None).await.0,
            StatusCode::NOT_FOUND,
            "{method} {path}"
        );
        // The same request signed for another server: refused, as any bad
        // signature, without a word about the chain.
        let header = keys.authorization(method, "sync.other", path, b"", now_ms());
        let req = digested(request(method, path, Some(&header), Body::empty()), b"");
        let (status, out) = send(&app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        assert_eq!(out["error"], "bad signature");
    }
}

#[sqlx::test]
async fn requests_captured_before_a_deletion_cannot_bring_the_chain_back(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = SyncCode::generate().keys();
    let path = format!("/collections/{}", keys.collection_id("sidebar"));
    let create = keys.authorization("POST", HOST, "/chain", b"", now_ms());
    let body = write_body(&keys, "sidebar", 0, b"{}").to_string();
    let write = keys.authorization("PUT", HOST, &path, body.as_bytes(), now_ms());
    let replay = |method, path: &str, auth: &str, body: &str| {
        digested(
            request(method, path, Some(auth), Body::from(body.to_owned())),
            body.as_bytes(),
        )
    };
    assert_eq!(
        send(&app, replay("POST", "/chain", &create, "")).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        call(&app, Some(&keys), "DELETE", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );

    assert_eq!(
        send(&app, replay("POST", "/chain", &create, "")).await.0,
        StatusCode::GONE
    );
    // A captured write finds the chain gone.
    assert_eq!(
        send(&app, replay("PUT", &path, &write, &body)).await.0,
        StatusCode::NOT_FOUND
    );
    // A repeated deletion learns that the chain is gone.
    assert_eq!(
        call(&app, Some(&keys), "DELETE", "/chain", None).await.0,
        StatusCode::NOT_FOUND
    );

    // Long after any captured signature has expired, the key may start over.
    sqlx::query("UPDATE tombstones SET deleted_at = deleted_at - make_interval(secs => $1)")
        .bind((TOMBSTONE_TTL + Duration::from_secs(60)).as_secs_f64())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&app, Some(&keys), "POST", "/chain", None).await.0,
        StatusCode::CREATED
    );
}

#[sqlx::test]
async fn servers_sharing_a_database_share_its_guards(pool: PgPool) {
    let limits = Limits {
        writes_per_minute: 2,
        ..Limits::default()
    };
    let one = app_with(&pool, Registration::Open, limits);
    let two = app_with(&pool, Registration::Open, limits);
    let keys = SyncCode::generate().keys();
    let create = keys.authorization("POST", HOST, "/chain", b"", now_ms());
    assert_eq!(
        send(
            &one,
            request("POST", "/chain", Some(&create), Body::empty())
        )
        .await
        .0,
        StatusCode::CREATED
    );

    // Writes count together, whichever server takes them.
    let path = format!("/collections/{}", keys.collection_id("sidebar"));
    for (app, base, status) in [
        (&one, 0, StatusCode::OK),
        (&two, 1, StatusCode::OK),
        (&one, 2, StatusCode::TOO_MANY_REQUESTS),
    ] {
        let body = write_body(&keys, "sidebar", base, b"{}");
        assert_eq!(
            call(app, Some(&keys), "PUT", &path, Some(body)).await.0,
            status
        );
    }

    // A chain deleted on one server cannot be brought back on the other.
    assert_eq!(
        call(&two, Some(&keys), "DELETE", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            &one,
            request("POST", "/chain", Some(&create), Body::empty())
        )
        .await
        .0,
        StatusCode::GONE
    );
}

#[sqlx::test]
async fn the_cap_on_chains_holds_when_servers_start_chains_at_once(pool: PgPool) {
    let limits = Limits {
        max_chains: 3,
        ..Limits::default()
    };
    let servers = [
        app_with(&pool, Registration::Open, limits),
        app_with(&pool, Registration::Open, limits),
    ];
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..32 {
        let app = servers[i % 2].clone();
        tasks.spawn(async move {
            let keys = SyncCode::generate().keys();
            call(&app, Some(&keys), "POST", "/chain", None).await.0
        });
    }
    let statuses = tasks.join_all().await;
    let created = statuses
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    assert_eq!(created, 3, "{statuses:?}");
}

#[sqlx::test]
async fn busy_servers_refuse_and_permits_last_until_responses_are_sent(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = SyncCode::generate().keys();
    call(&app, Some(&keys), "POST", "/chain", None).await;

    // Sixteen responses nobody has read yet hold all permits.
    let mut unread = Vec::new();
    for _ in 0..16 {
        let res = app
            .clone()
            .oneshot(signed(Some(&keys), "GET", "/chain", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        unread.push(res);
    }
    let start = std::time::Instant::now();
    let (status, out) = call(&app, Some(&keys), "GET", "/chain", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(out["error"].as_str().unwrap().contains("busy"));
    assert!(start.elapsed() >= Duration::from_secs(2), "waited briefly");
    // The health check is outside the gate.
    let health = Request::get("/v2/health").body(Body::empty()).unwrap();
    assert_eq!(send(&app, health).await.0, StatusCode::OK);

    drop(unread);
    assert_eq!(
        call(&app, Some(&keys), "GET", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );
}

/// The gate alone, in front of handlers that read the whole body as the
/// signed routes do: deadlines without a database, on a paused clock.
fn gated() -> Router {
    let inner = Router::new()
        .route("/chain", post(|_: Bytes| async { StatusCode::NO_CONTENT }))
        .route(
            "/collections/{id}",
            put(|_: Bytes| async { StatusCode::NO_CONTENT }),
        )
        .layer(middleware::from_fn_with_state(
            Arc::new(Gate::new()),
            gate::admit,
        ));
    Router::new().nest("/v2", inner)
}

#[tokio::test(start_paused = true)]
async fn small_requests_must_arrive_quickly() {
    let (sender, body) = stalled();
    let start = tokio::time::Instant::now();
    assert_eq!(
        send(&gated(), request("POST", "/chain", None, body))
            .await
            .0,
        StatusCode::REQUEST_TIMEOUT
    );
    assert!((SMALL_TIMEOUT..WRITE_TIMEOUT).contains(&start.elapsed()));
    drop(sender);
}

#[tokio::test(start_paused = true)]
async fn stalled_uploads_time_out() {
    let (sender, body) = stalled();
    let start = tokio::time::Instant::now();
    assert_eq!(
        send(&gated(), request("PUT", "/collections/x", None, body))
            .await
            .0,
        StatusCode::REQUEST_TIMEOUT
    );
    assert!(start.elapsed() >= WRITE_TIMEOUT);
    drop(sender);
}

#[sqlx::test]
async fn registration_modes(pool: PgPool) {
    let keys = SyncCode::generate().keys();
    let closed = app(&pool, Registration::Closed);
    assert_eq!(
        call(&closed, Some(&keys), "POST", "/chain", None).await.0,
        StatusCode::FORBIDDEN
    );
    let invite = app(&pool, Registration::Invite("secret".into()));
    assert_eq!(
        call(
            &invite,
            Some(&keys),
            "POST",
            "/chain",
            Some(serde_json::json!({"invite": "nope"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &invite,
            Some(&keys),
            "POST",
            "/chain",
            Some(serde_json::json!({"invite": "secret"}))
        )
        .await
        .0,
        StatusCode::CREATED
    );
}

#[test]
fn registration_from_env_is_closed_by_default_and_never_logs_the_invite_code() {
    let invite = Registration::from_env("invite:hunter2");
    assert_eq!(invite, Registration::Invite("hunter2".into()));
    assert!(!format!("{invite:?}").contains("hunter2"));
    assert_eq!(Registration::from_env("open"), Registration::Open);
    assert_eq!(Registration::from_env(""), Registration::Closed);
    assert_eq!(Registration::from_env("invite:"), Registration::Closed);
    assert_eq!(Registration::from_env("typo"), Registration::Closed);
}

#[sqlx::test]
async fn new_chains_per_day_are_limited(pool: PgPool) {
    let app = app_with(
        &pool,
        Registration::Open,
        Limits {
            max_new_chains_per_day: 2,
            ..Limits::default()
        },
    );
    let first = SyncCode::generate().keys();
    for keys in [&first, &SyncCode::generate().keys()] {
        assert_eq!(
            call(&app, Some(keys), "POST", "/chain", None).await.0,
            StatusCode::CREATED
        );
    }
    let (status, out) = call(
        &app,
        Some(&SyncCode::generate().keys()),
        "POST",
        "/chain",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(out["error"].as_str().unwrap().contains("today"));
    assert_eq!(
        call(&app, Some(&first), "POST", "/chain", None).await.0,
        StatusCode::NO_CONTENT,
        "existing chains are not new"
    );
    // A deletion does not give today's start back.
    assert_eq!(
        call(&app, Some(&first), "DELETE", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &app,
            Some(&SyncCode::generate().keys()),
            "POST",
            "/chain",
            None
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[sqlx::test]
async fn ready_answers_while_the_database_does(pool: PgPool) {
    let app = app(&pool, Registration::Closed);
    let ready = || Request::get("/v2/ready").body(Body::empty()).unwrap();
    assert_eq!(send(&app, ready()).await.0, StatusCode::OK);
    pool.close().await;
    // The answer is kept for a second.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(send(&app, ready()).await.0, StatusCode::SERVICE_UNAVAILABLE);
    let health = Request::get("/v2/health").body(Body::empty()).unwrap();
    assert_eq!(send(&app, health).await.0, StatusCode::OK, "still alive");
}

#[sqlx::test]
async fn writes_per_chain_and_minute_are_limited(pool: PgPool) {
    let app = app_with(
        &pool,
        Registration::Open,
        Limits {
            writes_per_minute: 2,
            ..Limits::default()
        },
    );
    let (keys, other) = (SyncCode::generate().keys(), SyncCode::generate().keys());
    let write = async |keys: &ChainKeys, base: u64| {
        let path = format!("/collections/{}", keys.collection_id("sidebar"));
        let body = write_body(keys, "sidebar", base, b"{}");
        call(&app, Some(keys), "PUT", &path, Some(body)).await.0
    };
    for k in [&keys, &other] {
        call(&app, Some(k), "POST", "/chain", None).await;
    }
    assert_eq!(write(&keys, 0).await, StatusCode::OK);
    assert_eq!(write(&keys, 1).await, StatusCode::OK);
    assert_eq!(write(&keys, 2).await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(write(&other, 0).await, StatusCode::OK, "per chain");
    exec(
        &pool,
        "UPDATE chains SET window_start = window_start - interval '61 seconds'",
    )
    .await;
    assert_eq!(write(&keys, 2).await, StatusCode::OK);
}

#[sqlx::test]
async fn collection_writes_are_checked_before_their_body_is_read(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = SyncCode::generate().keys();
    let path = format!("/collections/{}", keys.collection_id("sidebar"));
    let huge = || Body::from(vec![b'x'; 25 * 1024 * 1024]);
    let header = keys.authorization("PUT", HOST, &path, b"", now_ms());

    // Answered while the body has not even arrived: without a signature,
    // without a Content-Digest, with a signature that does not cover the
    // digest given, and (only the key holder learns it) for a chain that
    // does not exist.
    let early = async |req: Request<Body>| {
        let start = std::time::Instant::now();
        let status = send(&app, req).await.0;
        assert!(start.elapsed() < Duration::from_secs(1), "{status}");
        status
    };
    let (_unsent, body) = stalled();
    assert_eq!(
        early(request("PUT", &path, None, body)).await,
        StatusCode::UNAUTHORIZED
    );
    let (_unsent, body) = stalled();
    assert_eq!(
        early(request("PUT", &path, Some(&header), body)).await,
        StatusCode::BAD_REQUEST
    );
    let (_unsent, body) = stalled();
    assert_eq!(
        early(digested(
            request("PUT", &path, Some(&header), body),
            b"other"
        ))
        .await,
        StatusCode::UNAUTHORIZED
    );
    let (_unsent, body) = stalled();
    assert_eq!(
        early(digested(request("PUT", &path, Some(&header), body), b"")).await,
        StatusCode::NOT_FOUND
    );

    // A known chain may upload a collection, up to its size, and only the
    // body its digest names; nothing big elsewhere.
    call(&app, Some(&keys), "POST", "/chain", None).await;
    assert_eq!(
        send(
            &app,
            digested(request("PUT", &path, Some(&header), huge()), b"")
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        send(
            &app,
            digested(request("PUT", &path, Some(&header), Body::from("{}")), b"")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let padded = serde_json::json!({ "invite": "x".repeat(8 * 1024) });
    assert_eq!(
        call(&app, Some(&keys), "POST", "/chain", Some(padded))
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[sqlx::test]
async fn chains_are_capped_in_number_collections_and_bytes(pool: PgPool) {
    let small = SyncCode::generate().keys().seal("x", 1, b"{}").unwrap();
    let one = u64::try_from(small.nonce.len() + small.ciphertext.len()).unwrap();
    let app = app_with(
        &pool,
        Registration::Open,
        Limits {
            max_chains: 2,
            max_collections: 2,
            max_chain_bytes: 3 * one,
            ..Limits::default()
        },
    );
    let keys = SyncCode::generate().keys();
    let write = async |name: &str, base: u64, plain: &[u8]| {
        let path = format!("/collections/{}", keys.collection_id(name));
        let body = write_body(&keys, name, base, plain);
        call(&app, Some(&keys), "PUT", &path, Some(body)).await
    };

    // Two chains fit; a third does not, an existing one still answers.
    assert_eq!(
        call(&app, Some(&keys), "POST", "/chain", None).await.0,
        StatusCode::CREATED
    );
    let other = SyncCode::generate().keys();
    assert_eq!(
        call(&app, Some(&other), "POST", "/chain", None).await.0,
        StatusCode::CREATED
    );
    let third = SyncCode::generate().keys();
    assert_eq!(
        call(&app, Some(&third), "POST", "/chain", None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, Some(&keys), "POST", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );

    // Two collections; a third is refused, rewriting one is fine.
    assert_eq!(write("a", 0, b"{}").await.0, StatusCode::OK);
    assert_eq!(write("b", 0, b"{}").await.0, StatusCode::OK);
    assert_eq!(write("c", 0, b"{}").await.0, StatusCode::FORBIDDEN);
    assert_eq!(write("a", 1, b"{}").await.0, StatusCode::OK);

    // A write that would take the chain over its quota is refused.
    assert_eq!(
        write("b", 1, &[b' '; 3000]).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    // Nonces are 24 bytes; nothing else is stored.
    let id = keys.collection_id("a");
    let body = serde_json::json!({"base_version": 2, "nonce": "A".repeat(4096), "ciphertext": small.ciphertext});
    assert_eq!(
        call(
            &app,
            Some(&keys),
            "PUT",
            &format!("/collections/{id}"),
            Some(body)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

/// Move chain `id`'s days into the past.
async fn age(db: &Db, id: &str, created_days_ago: i64, seen_days_ago: i64) {
    sqlx::query(
        "UPDATE chains SET created_day = created_day - $2, last_seen_day = last_seen_day - $3
         WHERE id = $1",
    )
    .bind(id)
    .bind(created_days_ago)
    .bind(seen_days_ago)
    .execute(db.pool())
    .await
    .unwrap();
}

#[sqlx::test]
async fn chains_that_never_stored_anything_expire_after_a_week(pool: PgPool) {
    let db = Db::from_pool(pool);
    let limits = Limits::default();
    for id in ["empty-old", "empty-new", "used"] {
        assert_eq!(db.create_chain(id, &limits).await.unwrap(), Create::Created);
    }
    let sealed = SyncCode::generate().keys().seal("x", 1, b"{}").unwrap();
    db.put_collection("used", "x", 0, &sealed, &limits)
        .await
        .unwrap();
    age(&db, "empty-old", 8, 0).await;
    age(&db, "empty-new", 6, 0).await;
    age(&db, "used", 8, 0).await;

    let purged = maintain(&db, None, today()).await.unwrap();
    assert_eq!(
        purged,
        Some(Purged {
            empty: 1,
            inactive: 0
        })
    );
    assert!(!db.chain_exists("empty-old").await.unwrap());
    assert!(db.chain_exists("empty-new").await.unwrap());
    assert!(db.chain_exists("used").await.unwrap());
}

#[sqlx::test]
async fn inactive_chains_are_purged_unless_the_clock_jumped(pool: PgPool) {
    let db = Db::from_pool(pool);
    let limits = Limits::default();
    let sealed = SyncCode::generate().keys().seal("x", 1, b"{}").unwrap();
    for id in ["stale", "active"] {
        db.create_chain(id, &limits).await.unwrap();
        db.put_collection(id, "x", 0, &sealed, &limits)
            .await
            .unwrap();
    }
    age(&db, "stale", 401, 401).await;

    // "Today" more than a week past the newest activity: a clock that jumped
    // ahead would otherwise delete every chain.
    assert_eq!(maintain(&db, Some(400), today() + 500).await.unwrap(), None);
    assert!(db.chain_exists("stale").await.unwrap());

    let purged = maintain(&db, Some(400), today()).await.unwrap();
    assert_eq!(
        purged,
        Some(Purged {
            empty: 0,
            inactive: 1
        })
    );
    assert!(!db.chain_exists("stale").await.unwrap());
    assert!(db.chain_exists("active").await.unwrap());
}

/// The README's "Data it stores", column by column: a new column needs a
/// reason there first.
#[sqlx::test]
async fn the_database_stores_only_what_the_readme_lists(pool: PgPool) {
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations'
         ORDER BY table_name COLLATE \"C\", ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let columns: Vec<String> = columns.iter().map(|(t, c)| format!("{t}.{c}")).collect();
    assert_eq!(
        columns,
        [
            "chain_starts.day",
            "chain_starts.started",
            "chains.id",
            "chains.created_day",
            "chains.last_seen_day",
            "chains.window_start",
            "chains.window_writes",
            "collections.chain_id",
            "collections.id",
            "collections.version",
            "collections.nonce",
            "collections.ciphertext",
            "tombstones.id",
            "tombstones.deleted_at",
        ]
    );
}

#[sqlx::test]
async fn the_sweep_forgets_ended_write_windows_and_expired_tombstones(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let (kept, gone) = (SyncCode::generate().keys(), SyncCode::generate().keys());
    for keys in [&kept, &gone] {
        call(&app, Some(keys), "POST", "/chain", None).await;
        let path = format!("/collections/{}", keys.collection_id("sidebar"));
        let body = write_body(keys, "sidebar", 0, b"{}");
        call(&app, Some(keys), "PUT", &path, Some(body)).await;
    }
    call(&app, Some(&gone), "DELETE", "/chain", None).await;
    let db = Db::from_pool(pool.clone());
    db.sweep().await.unwrap();
    let left = || async {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT (SELECT count(*) FROM chains WHERE window_start IS NOT NULL),
                    (SELECT count(*) FROM tombstones)",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(left().await, (1, 1), "this minute's are kept");
    exec(
        &pool,
        "UPDATE chains SET window_start = window_start - interval '2 minutes'",
    )
    .await;
    sqlx::query("UPDATE tombstones SET deleted_at = deleted_at - make_interval(secs => $1)")
        .bind((TOMBSTONE_TTL + Duration::from_secs(60)).as_secs_f64())
        .execute(&pool)
        .await
        .unwrap();
    db.sweep().await.unwrap();
    assert_eq!(left().await, (0, 0));
}

#[sqlx::test]
async fn a_chain_writes_one_collection_at_a_time(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = SyncCode::generate().keys();
    call(&app, Some(&keys), "POST", "/chain", None).await;
    let path = format!("/collections/{}", keys.collection_id("sidebar"));
    // A write whose body is still on its way.
    let (sender, body) = stalled();
    let auth = keys.authorization("PUT", HOST, &path, b"", now_ms());
    let first = tokio::spawn({
        let (app, path) = (app.clone(), path.clone());
        async move {
            let req = digested(request("PUT", &path, Some(&auth), body), b"");
            send(&app, req).await.0
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let body = write_body(&keys, "sidebar", 0, b"{}");
    let (status, out) = call(&app, Some(&keys), "PUT", &path, Some(body.clone())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{out}");
    // Another chain is not held up.
    let other = SyncCode::generate().keys();
    call(&app, Some(&other), "POST", "/chain", None).await;
    let other_path = format!("/collections/{}", other.collection_id("sidebar"));
    let other_body = write_body(&other, "sidebar", 0, b"{}");
    assert_eq!(
        call(&app, Some(&other), "PUT", &other_path, Some(other_body))
            .await
            .0,
        StatusCode::OK
    );
    drop(sender);
    first.await.unwrap();
    assert_eq!(
        call(&app, Some(&keys), "PUT", &path, Some(body)).await.0,
        StatusCode::OK,
        "the next write may go once the first ended"
    );
}

#[sqlx::test]
async fn collection_transfers_cannot_hold_up_small_requests(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = SyncCode::generate().keys();
    call(&app, Some(&keys), "POST", "/chain", None).await;
    let path = format!("/collections/{}", keys.collection_id("sidebar"));
    let body = write_body(&keys, "sidebar", 0, b"{}");
    call(&app, Some(&keys), "PUT", &path, Some(body)).await;
    // Four collection reads nobody has received yet hold their slots.
    let mut unread = Vec::new();
    for _ in 0..4 {
        let res = app
            .clone()
            .oneshot(signed(Some(&keys), "GET", &path, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        unread.push(res);
    }
    assert_eq!(
        call(&app, Some(&keys), "GET", &path, None).await.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "a fifth waits, then is told the server is busy"
    );
    let start = std::time::Instant::now();
    assert_eq!(
        call(&app, Some(&keys), "GET", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "small requests go on"
    );
    drop(unread);
}

/// With the server's own session settings: a lock held elsewhere (a frozen
/// process, a migration) makes it answer "busy" within seconds.
#[sqlx::test]
async fn a_held_lock_makes_the_server_busy_not_stuck(_: PgPoolOptions, options: PgConnectOptions) {
    let db = Db::connect_with(options.clone()).await.unwrap();
    let app = router(AppState {
        db,
        registration: Registration::Open,
        retention_days: None,
        limits: Limits::default(),
    });
    let mut holder = sqlx::PgConnection::connect_with(&options).await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(CHAINS_LOCK)
        .execute(&mut holder)
        .await
        .unwrap();
    let start = std::time::Instant::now();
    let keys = SyncCode::generate().keys();
    assert_eq!(
        call(&app, Some(&keys), "POST", "/chain", None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    holder.close().await.unwrap();
    assert_eq!(
        call(&app, Some(&keys), "POST", "/chain", None).await.0,
        StatusCode::CREATED
    );
}

#[test]
fn database_urls_are_checked_without_being_quoted() {
    for url in [
        // sqlx would log these with their values.
        "postgres://u:p@127.0.0.1:1/db?sslpassword=secret",
        "postgres://u:p@127.0.0.1:1/db?sslmode=disable&connect_timeout=3",
        // A password with an unencoded `?`: its rest would read as a parameter.
        "postgres://u:pass?word-tail@127.0.0.1:1/db",
        "mysql://u:p@127.0.0.1:1/db",
        // No TLS in this build: a mode that needs it could never connect.
        "postgres://u:p@127.0.0.1:1/db?sslmode=require",
    ] {
        let error = format!("{:#}", Db::options(url).err().unwrap());
        assert!(error.contains("MAGNETITE_SYNC_DATABASE_URL"), "{error}");
        for part in ["secret", ":p@", "word-tail", "connect_timeout"] {
            assert!(!error.contains(part), "{error}");
        }
    }
    assert!(
        Db::options("postgres://u:p%3Fw@127.0.0.1:1/db?sslmode=disable&application_name=sync")
            .is_ok()
    );
}

/// The signature comes first: a small request whose body is slow to come
/// waits for it outside the gate, `SMALL_TIMEOUT` at most.
#[tokio::test(start_paused = true)]
async fn small_bodies_must_arrive_before_the_gate() {
    let verified = Router::new().nest(
        "/v2",
        Router::new()
            .route("/chain", post(|_: Bytes| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn(crate::api::verify)),
    );
    let keys = SyncCode::generate().keys();
    let auth = keys.authorization("POST", HOST, "/chain", b"", now_ms());
    let (sender, body) = stalled();
    let start = tokio::time::Instant::now();
    assert_eq!(
        send(&verified, request("POST", "/chain", Some(&auth), body))
            .await
            .0,
        StatusCode::REQUEST_TIMEOUT
    );
    assert!((SMALL_TIMEOUT..WRITE_TIMEOUT).contains(&start.elapsed()));
    drop(sender);
}

#[sqlx::test]
async fn requests_whose_signature_is_not_checked_yet_hold_no_slot(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let owner = SyncCode::generate().keys();
    call(&app, Some(&owner), "POST", "/chain", None).await;
    // More slow small requests than there are small slots, from keys of
    // anyone's making: their bodies never come.
    let mut unsent = Vec::new();
    let mut slow = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let keys = SyncCode::generate().keys();
        let auth = keys.authorization("POST", HOST, "/chain", b"", now_ms());
        let (sender, body) = stalled();
        unsent.push(sender);
        let app = app.clone();
        slow.spawn(async move {
            send(&app, request("POST", "/chain", Some(&auth), body))
                .await
                .0
        });
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let start = std::time::Instant::now();
    assert_eq!(
        call(&app, Some(&owner), "GET", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    drop(unsent);
    slow.join_all().await;
}

#[sqlx::test]
async fn waiting_transfers_leave_the_small_requests_alone(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = Arc::new(SyncCode::generate().keys());
    call(&app, Some(&keys), "POST", "/chain", None).await;
    let path = format!("/collections/{}", keys.collection_id("sidebar"));
    let body = write_body(&keys, "sidebar", 0, b"{}");
    call(&app, Some(&keys), "PUT", &path, Some(body)).await;
    // The four collection slots taken, and more transfers than may wait.
    let mut unread = Vec::new();
    for _ in 0..4 {
        unread.push(
            app.clone()
                .oneshot(signed(Some(&keys), "GET", &path, None))
                .await
                .unwrap(),
        );
    }
    let mut waiting = tokio::task::JoinSet::new();
    for _ in 0..70 {
        let (app, keys, path) = (app.clone(), Arc::clone(&keys), path.clone());
        waiting.spawn(async move { call(&app, Some(&keys), "GET", &path, None).await.0 });
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let start = std::time::Instant::now();
    assert_eq!(
        call(&app, Some(&keys), "GET", "/chain", None).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    let statuses = waiting.join_all().await;
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::SERVICE_UNAVAILABLE),
        "{statuses:?}"
    );
    drop(unread);
}

#[sqlx::test]
async fn only_collection_writes_upload_much(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = SyncCode::generate().keys();
    call(&app, Some(&keys), "POST", "/chain", None).await;
    let big = serde_json::json!({ "x": "x".repeat(8 * 1024) });
    assert_eq!(
        call(&app, Some(&keys), "PUT", "/chain", Some(big)).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[sqlx::test]
async fn requests_that_take_no_body_may_not_send_one(pool: PgPool) {
    let app = app(&pool, Registration::Open);
    let keys = SyncCode::generate().keys();
    call(&app, Some(&keys), "POST", "/chain", None).await;
    for method in ["GET", "DELETE"] {
        let mut req = signed(Some(&keys), method, "/chain", None);
        req.headers_mut()
            .insert("content-length", "5".parse().unwrap());
        *req.body_mut() = Body::from("hello");
        assert_eq!(send(&app, req).await.0, StatusCode::BAD_REQUEST, "{method}");
    }
}
