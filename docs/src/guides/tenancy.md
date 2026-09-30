# Configure tenancy and keys

This guide shows how to write a control-plane file, mint keys, and manage tenancy through the admin API. [Control plane](../concepts/control-plane.md) explains why each part works the way it does.

## Before you start

You need a roundhouse binary that starts (see [Getting started](getting-started.md)), a catalog in `ROUNDHOUSE_CATALOG` (see [Configure providers and the catalog](catalog.md)), and `sha256sum`. Without a control-plane file, roundhouse runs in Open mode and needs no key.

## Copy the example file

Copy `examples/control-plane.example.json` to a path outside the repository. The loader reads nothing in its `$comment` array.

The hashes in the example are placeholders that authenticate nothing, so every request gets `401 unknown_key`. The example's `acme` project names `local/REPLACE-with-your-local-model` and a frontier cadence. Both need a local fleet. The shipped `roundhouse` binary attaches no local fleet, so it refuses to boot with this file unchanged. Remove the local tier and the cadence, or attach a fleet.

## Know the file structure

The top level has these fields:

| Field | Required | Meaning |
|---|---|---|
| `projects` | yes | The project entries. |
| `users` | yes | The user entries. |
| `keys` | no | The turn keys. Each key names one membership. |
| `admin_keys` | no | SHA-256 hashes of admin secrets. |
| `credentials` | no | The deployment's own provider keys. Each entry names an environment variable. |
| `admission_cache_ttl_ms` | no | How long a node serves a compiled plane before it re-reads the directory. Default 30000. |
| `arm_salt` | no | The salt for validation arm assignment. See [Validate and steer](../concepts/validate-steer.md#arms). |
| `learner_recovery` | no | Required when a project enables the learner. See [The routing learner](../concepts/routing-learner.md). |
| `$comment` | no | Prose. Nothing reads it. |

A project entry has these fields:

| Field | Meaning |
|---|---|
| `id` | Required. Matches `^[a-z0-9][a-z0-9_-]{0,63}$`. A user `id` has the same pattern. |
| `name` | A label for operators. |
| `policy` | `min_quality`, `allow`, `frontier_cadence`. Absent means unrestricted. |
| `budget` | The spend ceiling. Absent means unlimited. |
| `fair_use` | Rolling windows. Absent means no rolling ceiling. |
| `credentials` | Whose provider keys the project's turns use: `mode` and `budget_counts`. |
| `validate` | The validate/steer loop. Absent means off. See [Validate and steer](../concepts/validate-steer.md#configuration). |
| `tiers` | Two-tier model selection. See [Choosing a model](../concepts/model-selection.md). |
| `learner` | The routing learner. See [The routing learner](../concepts/routing-learner.md). |

A key entry has these fields:

| Field | Meaning |
|---|---|
| `project` | Required. A declared project id. |
| `user` | Required. A declared user id. |
| `key_sha256` | Required. 64 lowercase hex characters. |
| `overrides` | Narrows the project policy for this key only. |
| `allocation` | This member's ceiling on top of the project budget. |
| `fair_use` | This member's own rolling windows. |
| `credentials` | This member's own provider keys. |

Every object refuses unknown fields. A misspelled field stops the boot and names the entry.

## Mint a key for the file

The file holds only the SHA-256 of each secret. Make the first admin secret yourself.

1. Make a secret with 43 random base62 characters:

   ```sh
   secret="rh_admin_$(LC_ALL=C tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 43)"
   ```

2. Hash the secret:

   ```sh
   printf '%s' "$secret" | sha256sum | cut -d' ' -f1
   ```

3. Put the hash in `admin_keys`.
4. Keep the secret in your secret manager. Roundhouse cannot show it again.

For a turn key in the file, use the prefix `rh_turn_` and put the hash in a `keys` entry. After the first boot, mint turn keys through the [admin API](#use-the-admin-api). Any other secret shape gets `401 malformed_key`.

## Point roundhouse at the file

Set `ROUNDHOUSE_CONTROL_PLANE` to the path of the file and start roundhouse. If the boot fails, read the error. It names the file and the entry. These are common refusals:

| Refusal | Cause |
|---|---|
| Unknown field | A misspelled field anywhere in the file. |
| Widening override | A key's `overrides` is wider than its project on an axis. |
| Cadence rations nothing | `max_frontier` is `0`. Write `"allow": ["local/*"]` instead. |
| Fair-use window caps nothing | A window has neither `max_tokens` nor `max_usd`, or a cap is zero. |
| Duplicate fair-use window | Two entries name the same window at one scope. |
| Overflow under `refuse` | `overflow_when_local_saturated` is set on a `refuse` budget. |
| No local capacity | A cadence, a local tier, or a degrade-mode budget with overflow off needs local models, and the deployment has none. |
| Floor above local quality | A key's floor makes the only local model inadmissible, but its cadence promises local service. |
| No judge | A project enables `validate`, and `ROUNDHOUSE_JUDGE_MODEL` names no catalog model. |
| `mcp_namespace` | The field is retired. Remove it. |
| `channel: "tool_call"` | The steer channel is retired. Use `text` or `auto`. |

## Present the key

A client sends a turn key in `x-roundhouse-key: rh_turn_…` or in `Authorization: Bearer rh_turn_…`. When both are present, `x-roundhouse-key` wins. Use it when `Authorization` carries a forwarded provider login. [Hook up Codex](codex.md) and [Hook up Claude Code](claude-code.md) show the client settings, and [Launch with topham](topham.md) writes them.

## Narrow a key's policy

Add `overrides` to a key entry, with the same three axes as a project `policy`:

```json
{
  "project": "acme",
  "user": "ada",
  "key_sha256": "…",
  "overrides": {
    "min_quality": 0.6,
    "frontier_cadence": { "max_frontier": 1, "per_turns": 10 }
  }
}
```

- An absent axis keeps the project's value. An axis can only narrow: a lower `min_quality` or a looser cadence is refused at load.
- `min_quality` is in `0.0..=1.0`. `frontier_cadence` needs `per_turns >= 1` and `1 <= max_frontier <= per_turns`.

## Set a budget

1. Add a `budget` block to the project:

   ```json
   "budget": {
     "limit_usd": 500.0,
     "window": "monthly",
     "on_exhaustion": "degrade_to_local",
     "overflow_when_local_saturated": true,
     "warn_at": 0.8
   }
   ```

2. Optionally, add an `allocation` to a key to give that member a second ceiling.

| Field | Values |
|---|---|
| `limit_usd` | Required. A positive number. |
| `window` | Required. `total` (the life of the project) or `monthly` (UTC calendar month). |
| `on_exhaustion` | Required. `degrade_to_local` or `refuse`. |
| `overflow_when_local_saturated` | Default `true`. Valid only with `degrade_to_local`. |
| `warn_at` | Default `0.8`, in `(0.0, 1.0]`. After this fraction of the limit, a grant is marked `Warned`. |

| Allocation | Meaning |
|---|---|
| absent or `"pooled"` | No second ceiling. The project limit still applies. |
| `{ "capped": { "limit_usd": 100.0 } }` | A fixed dollar ceiling for this member. |
| `{ "share": { "fraction": 0.25 } }` | A fraction of the project limit in `(0.0, 1.0]`. It follows a later change to the limit. |

Shares can sum past 1.0, and the project limit stops all members together. An `allocation` on a key whose project has no budget has no effect. The admin API refuses a change of `window` with `400 window_change_unsupported`.

## Set fair-use windows

Add a `fair_use` block to a project, a key, or both:

```json
"fair_use": {
  "windows": [
    { "window": "5h", "max_tokens": 2000000 },
    { "window": "7d", "max_usd": 50.0 }
  ]
}
```

- `window` is `5h`, `24h`, or `7d`. Name each window once at each scope.
- Each window needs `max_tokens`, `max_usd`, or both. Each cap must be positive.
- A key's windows are a second ceiling. They do not replace the project's windows.

A turn over a window gets HTTP 429 `fair_use_exceeded`. [Control plane](../concepts/control-plane.md#the-429) describes the body. For more than one node, set `ROUNDHOUSE_REDIS_URL`. Without it, each node counts in its own memory and logs a warning the first time it enforces a ceiling.

## Use the admin API

Send an admin key on every admin request, as `Authorization: Bearer rh_admin_…`. The admin API refuses every request in Open mode.

| Method and path | Action |
|---|---|
| `POST /v1/admin/projects` | Create a project. The body is a project entry. |
| `GET /v1/admin/projects` | List projects. |
| `GET /v1/admin/projects/{project}` | Read a project. |
| `PATCH /v1/admin/projects/{project}` | Change `name`, `policy`, `budget`, `fair_use`, `validate`, or `credentials`. |
| `DELETE /v1/admin/projects/{project}` | Archive the project. Returns 204. |
| `GET /v1/admin/projects/{project}/budget` | The reconciliation view. |
| `GET /v1/admin/projects/{project}/members` | List memberships. |
| `PUT /v1/admin/projects/{project}/members/{user}` | Create or replace a membership. |
| `DELETE /v1/admin/projects/{project}/members/{user}` | Delete a membership and revoke its keys. |
| `POST /v1/admin/projects/{project}/members/{user}/keys` | Mint a turn key. |
| `POST /v1/admin/users` | Create a user. The body is `{"id": "…"}`. |
| `GET /v1/admin/users` | List users. |
| `POST /v1/admin/keys` | Mint an admin key. |
| `GET /v1/admin/keys` | List keys. |
| `GET /v1/admin/keys/{key_id}` | Read a key. |
| `DELETE /v1/admin/keys/{key_id}` | Revoke a key. Returns 204. |

The list routes return `{"data": [...]}`. Each row has a `provenance` of `config` or `admin`. A change to a `config` row gets `409 config_owned`. Edit the file for those rows.

### Add a member and mint a turn key

1. Create the user:

   ```sh
   curl -sS -X POST http://127.0.0.1:8080/v1/admin/users \
     -H "Authorization: Bearer $ROUNDHOUSE_ADMIN_KEY" \
     -H 'content-type: application/json' \
     -d '{"id": "dana"}'
   ```

2. Create the membership. `role` is `owner` or `member`:

   ```sh
   curl -sS -X PUT http://127.0.0.1:8080/v1/admin/projects/acme/members/dana \
     -H "Authorization: Bearer $ROUNDHOUSE_ADMIN_KEY" \
     -H 'content-type: application/json' \
     -d '{"role": "member", "allocation": {"share": {"fraction": 0.25}}}'
   ```

3. Mint the key:

   ```sh
   curl -sS -X POST http://127.0.0.1:8080/v1/admin/projects/acme/members/dana/keys \
     -H "Authorization: Bearer $ROUNDHOUSE_ADMIN_KEY"
   ```

4. Copy the `secret` field from the response. Roundhouse returns it once.

A `PUT` replaces the membership. An absent `allocation` or `overrides` removes that ceiling or narrowing from all of the member's keys. A membership that the file declares gets `409 config_owned`.

`topham mint --profile <p> --project <P> --user <U>` does step 3. It reads the admin key from `ROUNDHOUSE_ADMIN_KEY`, prints an export line, and writes nothing to disk. See [Launch with topham](topham.md).

### Revoke a key

1. Find the key's `id` with `GET /v1/admin/keys`. The `display_tail` field shows the last four characters of the secret. It is `null` for a key from the file.
2. Send `DELETE /v1/admin/keys/{key_id}`.

The key stops on this node at the next request. Other nodes stop it within `admission_cache_ttl_ms`, or two TTLs if a directory refresh failed. A turn that is already streaming finishes.

You cannot revoke a key that the file declares, turn or admin. Remove its hash from the file and restart.

### Change a project

Send only the fields to change. An absent field stays as it is.

```sh
curl -sS -X PATCH http://127.0.0.1:8080/v1/admin/projects/acme \
  -H "Authorization: Bearer $ROUNDHOUSE_ADMIN_KEY" \
  -H 'content-type: application/json' \
  -d '{"budget": {"limit_usd": 800.0, "window": "monthly", "on_exhaustion": "degrade_to_local"}}'
```

An explicit `null` is refused, and the error names the field. The API cannot remove a block, because a removal widens a ceiling. To remove a block, edit the file.

### Archive a project

`DELETE /v1/admin/projects/{project}` archives the project, and its keys then get `403 project_archived`. No route undoes an archive or deletes a project.

## Tune revocation and durability

A smaller `admission_cache_ttl_ms` makes a revocation reach other nodes sooner and costs a store read more often. `0` re-reads on every request. Without `ROUNDHOUSE_REDIS_URL`, rows that the admin API creates are in process memory and are lost at restart, while rows from the file stay. See [Deploy with Redis](../operations/redis.md).
