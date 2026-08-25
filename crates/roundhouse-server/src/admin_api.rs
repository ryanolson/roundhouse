// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The admin plane, mounted as this deployment's fifth router.
//!
//! The only surface that *writes* tenancy. Everything it writes goes through
//! [`ControlDirectory`], which expresses an admin-created project in the same
//! vocabulary `ROUNDHOUSE_CONTROL_PLANE` does and compiles the two halves
//! together through the one compiler — so this file holds no rule about what a
//! valid project is, and could not disagree with the file if it tried. What is
//! here is the HTTP shape of those writes, the refusals that are about *this
//! request* rather than about the resulting state, and one read that exists
//! nowhere else: the reconciliation view.
//!
//! # Mode first, then the key
//!
//! [`admin_auth_layer`] refuses [`ControlPlane::Open`] before it looks at a
//! header, and the order is the point. In open mode every request resolves to
//! the built-in membership *with no key at all*, so a surface that authenticated
//! first would hand an unauthenticated caller a `KeyScope::Turn` and refuse it
//! as `wrong_key_kind` — an answer that says "use a different key" to a
//! deployment where no key exists and none can be issued, because the file is
//! the only root of trust an admin key can come from. See
//! [`AuthError::AdminRequiresControlPlane`].
//!
//! # What a mutation affects
//!
//! The next admission, and nothing in flight. A turn resolves its policy, its
//! budget and its credentials once, at admission, and holds them for its whole
//! life — so a key revoked while a turn is streaming does not interrupt that
//! turn, and a budget raised mid-turn does not raise that turn's ceiling. That
//! is emergent from per-request resolution rather than built here, and it is
//! written down because the alternative reading ("revocation kills live turns")
//! is the one an operator will assume.
//!
//! # What this milestone deliberately does not have
//!
//! No audit trail, no key rotation, no rate limiting, no pagination, no
//! rate-card editing, and no credential CRUD — see [`refuse_credentials`],
//! which is the one route that exists purely to say so.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use roundhouse_core::control::SpendLedger;
use roundhouse_core::metrics::{MetricsConfig, MetricsRecorder};
use roundhouse_core::now_ms;

use crate::control_config::{
    AllocationConfig, ApiKeyRecord, AuthError, ControlDirectory, ControlPlane, DirectoryMutation,
    DirectoryView, FairUseConfig, KeyRecordScope, KeyScope, MembershipRecord, MembershipRole,
    PolicyConfig, ProjectEntry, ProjectPatch, ProjectRecord, UserEntry, UserRecord,
};
use crate::http::{ApiError, parse_body};

mod reconciliation;
use reconciliation::budget_view;

/// Everything the admin routes read and write a deployment through.
///
/// Assembled at the composition root from the same values the other four
/// surfaces were built with. The recorder in particular: the reconciliation
/// view's `measured` column is folded out of the same log the dashboard folds,
/// and a second recorder here would produce a view that reconciles the ledger
/// against traffic nobody served.
pub(super) struct AdminState {
    pub(super) directory: Arc<ControlDirectory>,
    pub(super) spend: Arc<dyn SpendLedger>,
    pub(super) metrics: Arc<MetricsRecorder>,
    pub(super) metrics_config: Arc<MetricsConfig>,
}

impl Clone for AdminState {
    /// Written out rather than derived: `Arc<dyn SpendLedger>` is `Clone`, but
    /// deriving would also demand it of a future field that is only ever shared.
    fn clone(&self) -> Self {
        Self {
            directory: Arc::clone(&self.directory),
            spend: Arc::clone(&self.spend),
            metrics: Arc::clone(&self.metrics),
            metrics_config: Arc::clone(&self.metrics_config),
        }
    }
}

/// The `/v1/admin` routes, gated on an admin key and on the deployment's mode.
///
/// REST-nested rather than flat, and the nesting is the authorization story as
/// much as the URL story: a membership and a turn key have no identity outside
/// the project they belong to, so `/projects/{p}/members/{u}/keys` is the only
/// spelling under which "mint a key" cannot be asked without naming whose.
///
/// `DELETE /v1/admin/projects/{p}` archives. There is no route that deletes a
/// project, and that is a decision rather than a gap — see
/// [`ProjectRecord::archived_at_ms`](crate::control_config::ProjectRecord).
pub fn admin_router(
    directory: Arc<ControlDirectory>,
    spend: Arc<dyn SpendLedger>,
    metrics: Arc<MetricsRecorder>,
    metrics_config: Arc<MetricsConfig>,
) -> Router {
    let state = AdminState {
        directory: Arc::clone(&directory),
        spend,
        metrics,
        metrics_config,
    };
    Router::new()
        .route(
            "/v1/admin/projects",
            post(create_project).get(list_projects),
        )
        .route(
            "/v1/admin/projects/{project}",
            get(get_project)
                .patch(patch_project)
                .delete(archive_project),
        )
        .route("/v1/admin/projects/{project}/budget", get(budget_view))
        .route("/v1/admin/projects/{project}/members", get(list_members))
        .route(
            "/v1/admin/projects/{project}/members/{user}",
            put(upsert_member).delete(delete_member),
        )
        .route(
            "/v1/admin/projects/{project}/members/{user}/keys",
            post(mint_turn_key),
        )
        .route("/v1/admin/users", post(create_user).get(list_users))
        .route("/v1/admin/keys", post(mint_admin_key).get(list_keys))
        .route("/v1/admin/keys/{key_id}", get(get_key).delete(revoke_key))
        .route("/v1/admin/credentials", post(refuse_credentials))
        .with_state(state)
        // On this router only. The other four admit turn keys, and a layer
        // merged across all of them would refuse every turn in the deployment.
        .layer(axum::middleware::from_fn_with_state(
            directory,
            admin_auth_layer,
        ))
}

/// Refuse anything that is not an admin key on a deployment that has one.
///
/// The mode is asked *first* — see the module doc. After that this is the same
/// [`ControlPlane::scope`] every other surface resolves through, with the arm
/// the other surfaces accept refused here and vice versa:
/// [`crate::mcp_api::auth_layer`] answers `wrong_key_kind` to an admin key for
/// the mirror-image reason, and neither surface has a key vocabulary of its own.
///
/// The concrete [`ControlDirectory`] rather than an
/// `Arc<dyn PlaneSource>`(crate::control_config::PlaneSource), and this is the
/// one surface for which that is right: the routes behind it *write* tenancy,
/// which no plane source can do, so the admin plane is the seam where the
/// abstraction the other four share would buy nothing and hide the one
/// capability that matters.
///
/// # Accepted: the plane this layer authorized against is not the plane a
/// handler reads
///
/// This layer resolves `directory.plane(now_ms())` once, to decide 403 or
/// pass. Every handler behind it then independently calls
/// `state.directory.view(now_ms())` or `.apply(...)` for its own work, which
/// resolves the directory again. Between those two resolutions is a window —
/// microseconds wide — in which a concurrent admin write (another admin
/// revoking the very key this request authenticated with, say) could land, so
/// a request can in principle be authorized against one snapshot and executed
/// against another.
///
/// Ruled accepted rather than fixed: exploiting it needs a second admin
/// issuing a same-instant revocation against this one's key, which has no
/// realistic attacker model here. If this is ever worth closing, the shape
/// is to resolve the plane once *here* and thread it to the handler through
/// request extensions, so a handler cannot reach for the directory
/// independently — the same defense-in-depth [`ControlDirectory`]'s own
/// write path already applies one layer down, where a mutation holds a
/// single lock across its own read-validate-commit rather than resolving the
/// directory twice.
async fn admin_auth_layer(
    State(directory): State<Arc<ControlDirectory>>,
    request: Request,
    next: Next,
) -> Response {
    let plane = directory.plane(now_ms());
    if matches!(plane.as_ref(), ControlPlane::Open) {
        return ApiError::from(AuthError::AdminRequiresControlPlane).into_response();
    }
    match plane.scope(request.headers()) {
        Ok(KeyScope::Admin) => next.run(request).await,
        // An admin acts on the deployment and a turn key acts from inside one
        // project; there is no narrowing under which the second becomes the
        // first, which is why this is 403 and not a scope to escalate.
        Ok(KeyScope::Turn(_)) => ApiError::from(AuthError::WrongKeyKind).into_response(),
        Err(error) => ApiError::from(error).into_response(),
    }
}

// ---------------------------------------------------------------------------
// What a listing shows
// ---------------------------------------------------------------------------

/// One project, as an operator reads it back.
///
/// A view type in this crate rather than `Serialize` on the record, and the
/// reason is `provenance`: the record's own fields are the *file's* vocabulary,
/// and who owns a row is a fact about this API. Deriving on the record would
/// also put a serializer on a type a durable store will one day need its own
/// encoding for, and the two would have to be kept from drifting.
#[derive(Debug, Serialize)]
struct ProjectDto {
    id: String,
    name: Option<String>,
    /// `config` or `admin` — which of the deployment's two sources of truth owns
    /// this row, and therefore whether a `PATCH` of it will be refused.
    provenance: String,
    created_at_ms: Option<u64>,
    archived_at_ms: Option<u64>,
    /// Whether this project's turns are metered at all. The limit itself is
    /// deliberately not echoed here: what a project may spend is answered by the
    /// budget view, beside what it *has* spent, and a limit shown alone is the
    /// number people quote without the one that matters.
    budgeted: bool,
    /// This project's rolling fair-use windows, echoed whole — not as a
    /// `fair_used: bool` mirroring `budgeted` above.
    ///
    /// The asymmetry is the point and G14 is the record of it. `budgeted` can
    /// be a flag because the budget *view* answers the follow-up question with
    /// spend beside the limit. There is no fair-use view: the ledger's rolling
    /// counters are not a balance anyone reads. So a flag here would answer
    /// "is there a ceiling" and leave "which one" unanswerable from any
    /// surface, which is exactly the operator's position this field exists to
    /// end — and the `PATCH` route accepts this same block back.
    ///
    /// Rendered as `null` rather than omitted when there is no block, like
    /// every other optional field on this view: "this project has no rolling
    /// ceiling" is an answer, and a field that vanishes is one a reader cannot
    /// tell from a field this deployment is too old to have.
    fair_use: Option<FairUseConfig>,
}

impl From<&ProjectRecord> for ProjectDto {
    fn from(record: &ProjectRecord) -> Self {
        Self {
            id: record.entry.id.clone(),
            name: record.entry.name.clone(),
            provenance: record.provenance.to_string(),
            created_at_ms: record.created_at_ms,
            archived_at_ms: record.archived_at_ms,
            budgeted: record.entry.budget.is_some(),
            fair_use: record.entry.fair_use.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct UserDto {
    id: String,
    provenance: String,
    created_at_ms: Option<u64>,
}

impl From<&UserRecord> for UserDto {
    fn from(record: &UserRecord) -> Self {
        Self {
            id: record.entry.id.clone(),
            provenance: record.provenance.to_string(),
            created_at_ms: record.created_at_ms,
        }
    }
}

#[derive(Debug, Serialize)]
struct MembershipDto {
    project: String,
    user: String,
    /// Absent for a file-declared membership, which has no role vocabulary to
    /// have written one in — never defaulted to `member`, which would be a fact
    /// this deployment invented and then displayed as if an operator had.
    role: Option<String>,
    provenance: String,
    created_at_ms: Option<u64>,
}

impl From<&MembershipRecord> for MembershipDto {
    fn from(record: &MembershipRecord) -> Self {
        Self {
            project: record.project.clone(),
            user: record.user.clone(),
            role: record.role.map(|role| role.to_string()),
            provenance: record.provenance.to_string(),
            created_at_ms: record.created_at_ms,
        }
    }
}

/// One key, as every read surface shows it — which is to say without its secret.
///
/// **There is no field a plaintext could go in**, and that is inherited rather
/// than remembered: [`ApiKeyRecord`] has none either, so a handler that wanted
/// to leak one would have nothing to read it from. The only type that ever holds
/// a secret is [`MintedKey`](crate::control_config::MintedKey), and the only
/// place it is rendered is [`MintedKeyDto`].
#[derive(Debug, Serialize)]
struct KeyDto {
    id: String,
    /// The hash, which is not a secret: it is written in plain sight in the
    /// control-plane file, and its preimage is 32 CSPRNG bytes. Shown so an
    /// operator can match a row here against a row there.
    key_sha256: String,
    /// The last four characters of the secret, or `null` for a file-declared key
    /// whose plaintext this deployment has never seen.
    display_tail: Option<String>,
    /// `turn` or `admin`.
    scope: String,
    /// The membership a turn key pays as; `null` for an admin key, which belongs
    /// to no project.
    project: Option<String>,
    user: Option<String>,
    provenance: String,
    created_at_ms: Option<u64>,
    revoked_at_ms: Option<u64>,
    /// This member's own rolling windows, the second ceiling that binds beside
    /// the project's — `null` for a key that declares none, and always `null`
    /// for an admin key, which pays for nothing.
    ///
    /// Read-only here, and only a file-declared key can carry one: the admin
    /// plane mints a key under a membership and has no route that writes a
    /// member window. Showing it anyway is what makes the *project* view's
    /// answer complete — a member refused while the project has room is
    /// otherwise a refusal no admin surface can explain.
    fair_use: Option<FairUseConfig>,
}

impl From<&ApiKeyRecord> for KeyDto {
    fn from(record: &ApiKeyRecord) -> Self {
        let (scope, project, user) = match &record.scope {
            KeyRecordScope::Turn { project, user } => {
                ("turn", Some(project.clone()), Some(user.clone()))
            }
            KeyRecordScope::Admin => ("admin", None, None),
        };
        Self {
            id: record.id.clone(),
            key_sha256: record.key_sha256.clone(),
            display_tail: record.display_tail.clone(),
            scope: scope.to_string(),
            project,
            user,
            provenance: record.provenance.to_string(),
            created_at_ms: record.created_at_ms,
            revoked_at_ms: record.revoked_at_ms,
            fair_use: record.fair_use.clone(),
        }
    }
}

/// A freshly minted key: the one response body in this deployment that carries a
/// secret.
///
/// The `secret` field exists on this type and on no other. It is built from a
/// [`MintedKey`](crate::control_config::MintedKey) that the directory returned
/// and stored nothing of, so the value is dropped with the response that carried
/// it — "returned once and never again" is a property of what the types can
/// hold, not of a handler that remembers not to log it.
#[derive(Debug, Serialize)]
struct MintedKeyDto {
    secret: String,
    #[serde(flatten)]
    key: KeyDto,
}

/// One entity per list route, so a client parses one envelope.
#[derive(Debug, Serialize)]
struct ListDto<T> {
    data: Vec<T>,
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

/// `POST /v1/admin/projects`
///
/// The body *is* a `"projects"` entry — see [`ProjectEntry`]. Not a
/// hand-written request struct that happens to have the same fields: a second
/// spelling of a project is the first step towards a policy an operator can
/// write in one place and not the other, which is the whole thing R1 exists to
/// prevent.
///
/// Which is also where the `422` on a misspelled field comes from: the entry is
/// `deny_unknown_fields`, so this route inherits the file's strictness rather
/// than restating it. It needs that more than the file does — nothing here
/// reads a project back (see [`ProjectDto`]), so a silently dropped
/// `credential` would never surface anywhere at all.
async fn create_project(
    State(state): State<AdminState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let entry: ProjectEntry = parse_body(&body)?;
    let id = entry.id.clone();
    let records = state
        .directory
        .apply(DirectoryMutation::CreateProject { entry }, now_ms())?;
    let created = records
        .projects
        .iter()
        .find(|project| project.entry.id == id)
        .ok_or_else(|| {
            ApiError::internal(
                "directory_inconsistent",
                "the project was created and is not in the records the write returned",
            )
        })?;
    Ok((StatusCode::CREATED, axum::Json(ProjectDto::from(created))).into_response())
}

/// `GET /v1/admin/projects`
///
/// File-declared projects and API-created ones in one list, each labelled. Two
/// lists would be the shape that lets an operator miss the half they cannot
/// edit until a `PATCH` refuses them.
async fn list_projects(State(state): State<AdminState>) -> Response {
    let view = state.directory.view(now_ms());
    axum::Json(ListDto {
        data: view
            .projects
            .iter()
            .map(ProjectDto::from)
            .collect::<Vec<_>>(),
    })
    .into_response()
}

async fn get_project(
    State(state): State<AdminState>,
    Path(project): Path<String>,
) -> Result<Response, ApiError> {
    let view = state.directory.view(now_ms());
    let record = find_project(&view, &project)?;
    Ok(axum::Json(ProjectDto::from(record)).into_response())
}

/// `PATCH /v1/admin/projects/{project}`
///
/// Every axis but the budget *window*, which is refused — see
/// [`DirectoryError::WindowChangeUnsupported`](crate::control_config::DirectoryError).
/// The patch recompiles and re-validates the whole control plane before it is
/// written, so a change that would stop this deployment starting is refused here
/// rather than discovered at the next restart.
///
/// An axis sent as an explicit `null` is refused too, naming the axis — an
/// absent field is "leave alone" and there is no spelling for "remove this
/// block", so a `null` has no honest reading left. See [`ProjectPatch`].
async fn patch_project(
    State(state): State<AdminState>,
    Path(project): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let patch: ProjectPatch = parse_body(&body)?;
    let records = state.directory.apply(
        DirectoryMutation::PatchProject {
            id: project.clone(),
            patch,
        },
        now_ms(),
    )?;
    let patched = records
        .projects
        .iter()
        .find(|record| record.entry.id == project)
        .ok_or_else(|| {
            ApiError::internal(
                "directory_inconsistent",
                "the project was patched and is not in the records the write returned",
            )
        })?;
    Ok(axum::Json(ProjectDto::from(patched)).into_response())
}

/// `DELETE /v1/admin/projects/{project}` — archive, never delete.
///
/// `204` rather than the archived row: there is nothing here a caller learns
/// from the body that the `GET` does not answer, and returning a project from a
/// `DELETE` invites reading the response as "here is what still exists".
async fn archive_project(
    State(state): State<AdminState>,
    Path(project): Path<String>,
) -> Result<Response, ApiError> {
    state
        .directory
        .apply(DirectoryMutation::ArchiveProject { id: project }, now_ms())?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Users and memberships
// ---------------------------------------------------------------------------

/// `POST /v1/admin/users` — the body is a `"users"` entry.
///
/// There is no `DELETE` for a user in M8, and its absence is deliberate: a user
/// is only ever encountered through a membership, so the question an operator
/// actually has ("stop this person spending") is answered by deleting the
/// membership, which cascades to their keys. Deleting the person themselves
/// would have to decide what happens to the memberships and the spend rows that
/// name them, and that decision has not been made.
async fn create_user(State(state): State<AdminState>, body: Bytes) -> Result<Response, ApiError> {
    let entry: UserEntry = parse_body(&body)?;
    let id = entry.id.clone();
    let records = state
        .directory
        .apply(DirectoryMutation::CreateUser { entry }, now_ms())?;
    let created = records
        .users
        .iter()
        .find(|user| user.entry.id == id)
        .ok_or_else(|| {
            ApiError::internal(
                "directory_inconsistent",
                "the user was created and is not in the records the write returned",
            )
        })?;
    Ok((StatusCode::CREATED, axum::Json(UserDto::from(created))).into_response())
}

async fn list_users(State(state): State<AdminState>) -> Response {
    let view = state.directory.view(now_ms());
    axum::Json(ListDto {
        data: view.users.iter().map(UserDto::from).collect::<Vec<_>>(),
    })
    .into_response()
}

/// The body of a membership `PUT`.
///
/// `allocation` and `overrides` are the file's own shapes; `role` is the one
/// field the file has no spelling for. Absent `allocation` is
/// [`Allocation::Pooled`] — no *second* ceiling, not no budget — and absent
/// `overrides` narrows nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MembershipBody {
    role: MembershipRole,
    #[serde(default)]
    allocation: Option<AllocationConfig>,
    #[serde(default)]
    overrides: Option<PolicyConfig>,
}

/// `PUT /v1/admin/projects/{p}/members/{u}` — create or replace.
///
/// An upsert rather than a `POST`/`PATCH` pair because a membership has no
/// identity of its own: the pair in the path *is* the identity, so the same body
/// sent twice has to mean the same thing, which is exactly what `PUT` promises.
///
/// **Replace, not merge.** The entitlements land on the membership and every one
/// of its keys is compiled from them, so an upsert that dropped `allocation`
/// removes the member ceiling from all of them at once — which is the honest
/// reading of `PUT`, and is why this route is spelled as one.
async fn upsert_member(
    State(state): State<AdminState>,
    Path((project, user)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: MembershipBody = parse_body(&body)?;
    let records = state.directory.apply(
        DirectoryMutation::UpsertMembership {
            project: project.clone(),
            user: user.clone(),
            role: request.role,
            allocation: request.allocation,
            overrides: request.overrides,
        },
        now_ms(),
    )?;
    let membership = records
        .memberships
        .iter()
        .find(|record| record.names(&project, &user))
        .ok_or_else(|| {
            ApiError::internal(
                "directory_inconsistent",
                "the membership was written and is not in the records the write returned",
            )
        })?;
    Ok(axum::Json(MembershipDto::from(membership)).into_response())
}

async fn list_members(
    State(state): State<AdminState>,
    Path(project): Path<String>,
) -> Result<Response, ApiError> {
    let view = state.directory.view(now_ms());
    find_project(&view, &project)?;
    Ok(axum::Json(ListDto {
        data: view
            .memberships
            .iter()
            .filter(|membership| membership.project == project)
            .map(MembershipDto::from)
            .collect::<Vec<_>>(),
    })
    .into_response())
}

/// `DELETE /v1/admin/projects/{p}/members/{u}`
///
/// Removes the edge and revokes every key minted under it. The cascade is not a
/// convenience: a key whose membership is gone resolves to no policy, no budget
/// and no principal, so leaving it live would be a secret that authenticates as
/// nothing.
async fn delete_member(
    State(state): State<AdminState>,
    Path((project, user)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    state.directory.apply(
        DirectoryMutation::DeleteMembership { project, user },
        now_ms(),
    )?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// `POST /v1/admin/projects/{p}/members/{u}/keys` — mint a turn key.
///
/// No body, and no per-key overrides: what this key may do is read off its
/// membership at every compile, so there is no second copy of a membership's
/// entitlements for two of its keys to disagree in. A file-declared key keeps
/// its own `overrides`, which is the file's prerogative and not this API's.
async fn mint_turn_key(
    State(state): State<AdminState>,
    Path((project, user)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let minted = state.directory.mint_turn_key(&project, &user, now_ms())?;
    let view = state.directory.view(now_ms());
    let record = find_key_by_hash(&view, &minted.key_sha256)?;
    Ok((
        StatusCode::CREATED,
        axum::Json(MintedKeyDto {
            secret: minted.secret,
            key: KeyDto::from(record),
        }),
    )
        .into_response())
}

/// `POST /v1/admin/keys` — mint an admin key.
async fn mint_admin_key(State(state): State<AdminState>) -> Result<Response, ApiError> {
    let minted = state.directory.mint_admin_key(now_ms())?;
    let view = state.directory.view(now_ms());
    let record = find_key_by_hash(&view, &minted.key_sha256)?;
    Ok((
        StatusCode::CREATED,
        axum::Json(MintedKeyDto {
            secret: minted.secret,
            key: KeyDto::from(record),
        }),
    )
        .into_response())
}

async fn list_keys(State(state): State<AdminState>) -> Response {
    let view = state.directory.view(now_ms());
    axum::Json(ListDto {
        data: view.keys.iter().map(KeyDto::from).collect::<Vec<_>>(),
    })
    .into_response()
}

async fn get_key(
    State(state): State<AdminState>,
    Path(key_id): Path<String>,
) -> Result<Response, ApiError> {
    let view = state.directory.view(now_ms());
    let record = view
        .keys
        .iter()
        .find(|key| key.id == key_id)
        .ok_or_else(|| ApiError::not_found("key_not_found", format!("no key `{key_id}`")))?;
    Ok(axum::Json(KeyDto::from(record)).into_response())
}

/// `DELETE /v1/admin/keys/{key_id}` — tombstone, never delete.
///
/// `204`, and idempotent: a second `DELETE` of an already-revoked key is the
/// same request arriving twice, and answering 404 to it would make a retry after
/// a dropped response look like a bug.
async fn revoke_key(
    State(state): State<AdminState>,
    Path(key_id): Path<String>,
) -> Result<Response, ApiError> {
    state
        .directory
        .apply(DirectoryMutation::RevokeKey { id: key_id }, now_ms())?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Credentials: the route that exists to refuse
// ---------------------------------------------------------------------------

/// `POST /v1/admin/credentials`
///
/// **There is no credential CRUD in this milestone**, and this route exists so
/// that the fact is discoverable from the API rather than from a 404 that reads
/// like a typo. A provider key is configured in `ROUNDHOUSE_CONTROL_PLANE`,
/// which names an environment variable this process holds; a sealed store that
/// this API could write into is deferred.
///
/// Two refusals rather than one, because the two mistakes are different sizes.
/// An OAuth-shaped body is a caller trying to hand roundhouse a *refresh token* —
/// a credential with a lifecycle, a revocation endpoint and a rotation
/// schedule — and that is refused on its own terms, permanently as far as this
/// milestone is concerned, rather than filed under "not yet". Anything else is
/// the ordinary "this build cannot do that", which is a 501.
///
/// Decided on the request's JSON shape and nowhere near the credential
/// resolution code, which this milestone deliberately does not touch.
async fn refuse_credentials(body: Bytes) -> ApiError {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        // A body this cannot read still gets the 501: the answer does not depend
        // on what it said, and reporting a parse error would suggest a
        // well-formed one would have worked.
        Err(_) => return credential_crud_unavailable(),
    };
    if is_oauth_shaped(&parsed) {
        return ApiError::bad_request(
            "oauth_credentials_unsupported",
            "an OAuth credential cannot be stored here. A refresh token is a credential with a \
             lifecycle -- it rotates, it is revoked upstream, and holding one means being \
             responsible for refreshing it -- and this deployment has no place to keep one. A \
             forwarded ChatGPT login is supported as pass-through, where the caller's own token \
             arrives with each turn and is never stored; a provider API key is configured in \
             ROUNDHOUSE_CONTROL_PLANE",
        );
    }
    credential_crud_unavailable()
}

fn credential_crud_unavailable() -> ApiError {
    ApiError::not_implemented(
        "credential_crud_not_available",
        "this build stores no provider credentials. A project's credentials are declared in the \
         file named by ROUNDHOUSE_CONTROL_PLANE, whose entries name environment variables this \
         process reads at load time; a sealed store this API could write into is deferred",
    )
}

/// The three field names only an OAuth payload has, in both spellings a client
/// library produces.
///
/// Both, rather than one and a normalization step, because the set is the whole
/// rule: a reader deciding whether a body will be refused should be able to
/// read the answer rather than simulate a transformation.
const OAUTH_FIELDS: [&str; 6] = [
    "refresh_token",
    "refreshToken",
    "id_token",
    "idToken",
    "client_id",
    "clientId",
];

/// Whether a body is asking to store an OAuth credential, at any depth.
///
/// The `kind` tag if any object carries one, and otherwise the field names that
/// only an OAuth payload has. Shape rather than tag alone, because the body a
/// caller actually sends is whatever their client library serialized, and a
/// refusal that only recognised `{"kind":"oauth"}` would answer 501 — "not yet"
/// — to the one request whose answer is "not like this".
///
/// **Recursive, and that follows from the same sentence.** A credentials block
/// in this deployment's own vocabulary nests per provider, so a caller who read
/// that format and tried to hand over a refresh token writes
/// `{"providers": {"anthropic": {"refresh_token": …}}}` — a top-level-keys-only
/// check answers "not yet" to it, which is the wrong half of the two refusals.
/// Arrays are walked for the same reason: a client that batches its credentials
/// is describing the same thing one level out.
///
/// The recursion is bounded by `serde_json`'s own parse depth limit, which the
/// body has already survived by the time this runs — there is no depth here a
/// request could choose.
fn is_oauth_shaped(body: &Value) -> bool {
    match body {
        Value::Object(fields) => {
            if fields.get("kind").and_then(Value::as_str) == Some("oauth") {
                return true;
            }
            fields.iter().any(|(name, value)| {
                OAUTH_FIELDS.contains(&name.as_str()) || is_oauth_shaped(value)
            })
        }
        Value::Array(items) => items.iter().any(is_oauth_shaped),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Lookups shared by the routes above
// ---------------------------------------------------------------------------

pub(super) fn find_project<'a>(
    view: &'a DirectoryView,
    id: &str,
) -> Result<&'a ProjectRecord, ApiError> {
    view.projects
        .iter()
        .find(|project| project.entry.id == id)
        .ok_or_else(|| ApiError::not_found("project_not_found", format!("no project `{id}`")))
}

/// The row a mint just wrote, found by the hash the mint returned.
///
/// By hash rather than by re-deriving the id, so that a change to the id's
/// shape cannot make a mint report a key that is not the one it stored.
fn find_key_by_hash<'a>(
    view: &'a DirectoryView,
    key_sha256: &str,
) -> Result<&'a ApiKeyRecord, ApiError> {
    view.keys
        .iter()
        .find(|key| key.key_sha256 == key_sha256)
        .ok_or_else(|| {
            ApiError::internal(
                "directory_inconsistent",
                "the key was minted and stored and is not in the view the write produced",
            )
        })
}
