//! `/api/v1/docs` (docs/docs.md §9). Every route takes `?realm=` (you;
//! a realm's caller stays in its own), else the caller's realm, else home.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use c6_api as api;
use c6_api::docs::{Doc, DocApplied, DocBatch, DocNew, DocPatch, DocQuery, DocRead, DocRestore, DocVersion};
use serde_json::Value;

use crate::HeadState;
use crate::api::ApiResult;
use crate::realms::Caller;

pub fn routes() -> Router<Arc<HeadState>> {
    Router::new()
        .route("/api/v1/docs", get(list).post(create))
        .route("/api/v1/docs/{id}", get(one).patch(patch).delete(remove))
        .route("/api/v1/docs/{id}/ops", post(ops))
        .route("/api/v1/docs/{id}/read", get(read))
        .route("/api/v1/docs/{id}/history", get(history))
        .route("/api/v1/docs/{id}/restore", post(restore))
}

/// `GET /api/v1/docs/{id}/history`.
#[derive(Clone, Debug, Default, schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct HistoryQuery {
    #[serde(default)]
    pub realm: Option<String>,
    /// Versions at most (newest first; default 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

async fn realm(st: &HeadState, caller: &Caller, r: &api::InRealm) -> anyhow::Result<String> {
    super::realm_for(st, caller, r.realm.as_deref()).await
}

async fn list(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Query(q): Query<DocQuery>,
) -> ApiResult<Json<Vec<api::docs::DocSummary>>> {
    let realm = realm(&st, &caller, &r).await?;
    Ok(Json(super::list(&st, &realm, &q).await?))
}

/// The new document, and the ids its blocks and `<?c6 block k?>` keys got.
#[derive(serde::Serialize)]
struct Created {
    #[serde(flatten)]
    doc: Doc,
    applied: DocApplied,
}

async fn create(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(n): Json<DocNew>,
) -> ApiResult<Json<Value>> {
    let realm = realm(&st, &caller, &r).await?;
    let (doc, applied) = super::create(&st, &realm, &n, &super::by_of(&caller)).await?;
    Ok(Json(serde_json::to_value(Created { doc, applied }).map_err(anyhow::Error::from)?))
}

async fn one(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Doc>> {
    let realm = realm(&st, &caller, &r).await?;
    Ok(Json(super::get(&st, &realm, &id).await?))
}

async fn patch(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(p): Json<DocPatch>,
) -> ApiResult<Json<Doc>> {
    let realm = realm(&st, &caller, &r).await?;
    Ok(Json(super::patch(&st, &realm, &id, &p, &super::by_of(&caller)).await?))
}

async fn remove(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<axum::http::StatusCode> {
    let realm = realm(&st, &caller, &r).await?;
    super::delete(&st, &realm, &id, &super::by_of(&caller)).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn ops(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(b): Json<DocBatch>,
) -> ApiResult<Json<DocApplied>> {
    let realm = realm(&st, &caller, &r).await?;
    Ok(Json(super::apply(&st, &realm, &id, &b, &super::by_of(&caller), None).await?))
}

async fn read(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(r): Query<api::InRealm>,
    Query(q): Query<DocRead>,
) -> ApiResult<Json<Value>> {
    let realm = realm(&st, &caller, &r).await?;
    Ok(Json(super::read(&st, &realm, &id, &q).await?))
}

async fn history(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<Vec<DocVersion>>> {
    let realm = super::realm_for(&st, &caller, q.realm.as_deref()).await?;
    Ok(Json(super::history(&st, &realm, &id, q.limit.unwrap_or(200)).await?))
}

async fn restore(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(b): Json<DocRestore>,
) -> ApiResult<Json<DocApplied>> {
    let realm = realm(&st, &caller, &r).await?;
    Ok(Json(super::restore(&st, &realm, &id, &b, &super::by_of(&caller)).await?))
}
