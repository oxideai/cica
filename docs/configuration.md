# Configuration

Cica reads `config.toml` from your platform config directory. Run `cica paths` to see the exact location (e.g. `~/Library/Application Support/cica/config.toml` on macOS, `~/.config/cica/config.toml` on Linux). `cica init` writes this file for you; you can also edit it by hand.

A handful of settings can be supplied (or overridden) by environment variables — this is how a cloud worker runs with **no `config.toml` at all**, taking everything from its task environment.

See [config.example.toml](../config.example.toml) for a copy-pasteable template.

## Minimal single-box config

```toml
backend = "claude"

[channels.telegram]
bot_token = "123456:ABC..."

[claude]
api_key = "sk-ant-..."
```

That's enough to run `cica`. Everything else is optional.

## Top-level keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `backend` | `"claude"` \| `"cursor"` | `"claude"` | Which AI backend to use. |
| `audit` | bool | `true` | Log conversations and system events to `audit.db`. |
| `onboarding_prompt` | string | — | Global onboarding prompt; a channel can override it. |

## Channels

Configure one or more. A channel section's presence is what enables it.

### `[channels.telegram]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `bot_token` | string | — | Telegram bot token. |
| `auto_approve` | bool | `false` | Auto-approve new users (skip `cica approve`). |
| `shared_identity` | bool | `false` | Use shared `PERSONA.md` instead of per-user identity/onboarding. |
| `onboarding_prompt` | string | — | Per-channel onboarding prompt override. |

### `[channels.signal]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `phone_number` | string | — | The Signal number cica registers as. |
| `auto_approve` | bool | `false` | Auto-approve new users. |
| `shared_identity` | bool | `false` | Use shared `PERSONA.md`. |
| `onboarding_prompt` | string | — | Per-channel override. |

### `[channels.slack]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `bot_token` | string | — | Slack bot token (`xoxb-...`). |
| `app_token` | string | — | Slack app-level token (`xapp-...`). |
| `auto_approve` | bool | `false` | Auto-approve new users. |
| `shared_identity` | bool | `false` | Use shared `PERSONA.md`. |
| `onboarding_prompt` | string | — | Per-channel override. |
| `unfurl_links` | bool | `false` | Let Slack preview links in bot messages. |

## Backends

### `[claude]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `api_key` | string | — | Anthropic API key or OAuth token (when not using Vertex). |
| `model` | string | — | Alias (`"sonnet"`, `"opus"`) or full model ID. |
| `use_vertex` | bool | `false` | Use Google Vertex AI instead of the Anthropic API. |
| `vertex_project_id` | string | — | GCP project ID (required when `use_vertex`). |
| `vertex_region` | string | `"europe-west1"` | GCP region for Vertex. |
| `vertex_credentials_path` | string | — | Path to a GCP service-account JSON key. When set, long-lived auth is used so no interactive `gcloud login` is needed — recommended for servers. |

### `[cursor]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `api_key` | string | — | Cursor API key. |
| `model` | string | `claude-sonnet-4-20250514` | Model to use. |

## Deployment (cloud mode)

All of `[deployment]` is optional; absent means single-box. See [architecture.md](architecture.md) for what each setting does.

### `[deployment]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `store` | `"filesystem"` \| `"s3"` | none | Durable state store. None disables hydration (pure local). |
| `state_path` | string | `internal/state-store` | Root path for the filesystem store. |
| `provider` | `"local"` \| `"subprocess"` \| `"docker"` \| `"fargate"` | `local` | Where a turn executes. |
| `docker_image` | string | `cica-worker:latest` | Worker image for `provider = "docker"`. |

### `[deployment.s3]` (when `store = "s3"`)
| Key | Type | Default | Meaning |
|---|---|---|---|
| `bucket` | string | — | Bucket name (required). |
| `region` | string | AWS chain | Region; falls back to the default chain. |
| `prefix` | string | — | Optional key namespace within the bucket. |
| `endpoint` | string | — | Endpoint override (LocalStack / MinIO / testing). |

> Credentials are **never** in config — they come from the standard AWS provider chain (env vars or instance/task IAM role).

### `[deployment.fargate]` (when `provider = "fargate"`)
Requires a build with `--features fargate`.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `cluster` | string | — | ECS cluster name or ARN (required). |
| `task_definition` | string | — | Task-def family or `family:revision` (required). |
| `subnets` | string[] | `[]` | awsvpc subnets to launch into (required in practice). |
| `security_groups` | string[] | `[]` | Security groups for the task. |
| `assign_public_ip` | bool | `false` | Assign a public IP (default: private subnets + NAT). |
| `region` | string | AWS chain | Region. |
| `container_name` | string | `cica-worker` | Which container in the task-def to override with `worker --turn <id>`. |
| `poll_interval_secs` | u64 | `5` | DescribeTasks poll interval. |
| `timeout_secs` | u64 | `900` | Max wait for the task to stop before bailing. |

## Skills git-sync

### `[skills]`
Absent means no skills sync (skills are read from the local folder only).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `repo` | string | — | Git repository URL of the skills repo (required). |
| `ref` | string | `"main"` | Branch, tag, or sha to check out. |
| `refresh_secs` | u64 | `600` | Seconds between re-pulls. |

> The git credential is read from the `CICA_SKILLS_GIT_TOKEN` env var — never from config.

## Environment variables

These overlay config at load time (env wins over file). The cloud worker uses these so it needs no `config.toml`.

| Variable | Overrides | Notes |
|---|---|---|
| `CICA_CLAUDE_API_KEY` | `claude.api_key` | |
| `CICA_CURSOR_API_KEY` | `cursor.api_key` | |
| `CICA_BACKEND` | `backend` | `claude` or `cursor`. |
| `CICA_STORE` | `deployment.store` | `s3` or `filesystem`. |
| `CICA_S3_BUCKET` | `deployment.s3.bucket` | |
| `CICA_S3_REGION` | `deployment.s3.region` | |
| `CICA_SKILLS_GIT_TOKEN` | — | Git credential for the skills sync loop. Env-only. |
| `CICA_LOG_JSON` | — | If set, emit logs as JSON (set to any value). |
| `RUST_LOG` | — | Standard tracing filter (e.g. `info`, `cica=debug`). |
| AWS chain (`AWS_*` / instance role) | — | S3 + Fargate credentials. Never in config. |

## What's secret

Cica does not integrate a secret manager itself — in single-box mode, tokens and keys sit in `config.toml` in plaintext, so protect that file. The secret-bearing fields:

- `claude.api_key`, `cursor.api_key`
- `channels.*.bot_token` / `app_token`
- `claude.vertex_credentials_path` (points at a credentials file)
- `CICA_SKILLS_GIT_TOKEN` (env)
- AWS credentials (via the provider chain)

In cloud mode, inject these as environment variables from your platform's secret store rather than baking a `config.toml` — see your deployment's runbook. (For Root's deployment, that's the `sprout` repo.)
