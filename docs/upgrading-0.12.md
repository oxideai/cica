# Upgrading to 0.12: warm workers on Fargate

Context for whoever edits the deployment. cica 0.12.0 (PRs #61, #63, #69) replaces the
one-task-per-turn model with one warm worker task per active session. The router launches a
task on a session's first turn, routes follow-up turns to it through the S3 state store, and
the task exits itself after `worker_idle_secs` (default 600) of inactivity or
`worker_max_age_secs` (default 86400). Nothing about networking, the state store bucket, or
credentials changes. The items below are the complete list.

## 1. Rollout order (the one that causes an outage if wrong)

Deploy the worker image before the router. A 0.12 router launches workers with
`cica worker --session ...`, which a 0.11 image does not understand. The reverse is safe: a
0.11 router still drives a 0.12 image through the retained `--turn` mode, so rolling the router
back alone is fine. Concretely: update the task definition to the 0.12.0 image and register the
revision first; then deploy the router container/service at 0.12.0.

## 2. IAM: the router's task role needs one more action

Add `ecs:ListTasks` (scoped to the cluster as the existing statements are). The router uses it
to rediscover a worker it started before a crash, via `startedBy`. Already required and
unchanged: `ecs:RunTask`, `ecs:DescribeTasks`, `ecs:StopTask`, `iam:PassRole` for the task's
execution and task roles. The worker task role is unchanged.

## 3. Worker task definition

- Do not hard-code a `command`; the router overrides it per launch (it already did for
  `--turn`). `container_name` in the router config must match the container in the task
  definition (default `cica-worker`).
- Tasks now live for hours. Remove or raise anything that assumed a task lasts one turn:
  external "stop tasks older than N minutes" automation, alarms on long-running tasks, and a
  `stopTimeout` shorter than 30 s (the router waits up to 30 s for a confirmed stop before it
  will launch a replacement).
- Worker environment: keep `CICA_STORE`, `CICA_S3_BUCKET`, `CICA_S3_REGION`, and the backend
  credential (`CICA_CLAUDE_API_KEY` or `CICA_CURSOR_API_KEY`). `CICA_BACKEND` and
  `CICA_*_MODEL` on the worker no longer choose what runs; the router's job does. If you set
  any `CICA_WORKER_*` / `CICA_TURN_TIMEOUT_SECS` on the router, set the same values on the
  worker; the router hands the worker its policy on the command line, and the worker reads
  only `worker_max_age_secs` from its own environment.
- `RunTask` now carries `clientToken` and `startedBy` (a 36-char launch token). No CDK change,
  but `startedBy` is how to find a session's task in the console or with
  `aws ecs list-tasks --cluster <c> --started-by <token>`.

## 4. Router configuration

- `[deployment.fargate] timeout_secs` is gone; its meaning moved to
  `[deployment] turn_timeout_secs` (default 900). Config parsing ignores unknown keys, so a
  leftover value is silently inert; move it if it was customised.
- New optional `[deployment]` keys, all with defaults: `worker_idle_secs` 600,
  `worker_start_timeout_secs` 180, `turn_timeout_secs` 900, `worker_cap` 32,
  `worker_max_age_secs` 86400. Each has a `CICA_...` env form (see docs/configuration.md).
  Changing one on the router changes the policy hash and makes it replace existing workers as
  they next take a turn.

## 5. S3 bucket

- Add a lifecycle rule expiring objects under `<prefix>/turns/` after 1 day. The router
  deletes a turn's records on the normal path; this is the backstop for a router that died
  mid-turn. Also harmless: a rule expiring `<prefix>/sessions/*/workers/` after 1 day.
- The tree layout (from 0.11) is `<key>/gen/<uuid>/...` plus a `current` manifest; keys migrate
  on first push, and old generations are pruned an hour after they stop being current. No
  action.

## 6. Capacity and cost

- Up to `worker_cap` (32) tasks per router can be warm at once. Check the account's Fargate
  task quota for the cluster covers that plus the router. When the cap is reached the router
  stops the least recently used idle worker; if none is idle, that turn fails with
  "all workers busy" until one frees up.
- Billing shifts from per-turn to per-warm-worker-minute. With the defaults an active session
  costs its turn time plus up to 10 idle minutes after its last message.

## 7. Verification after deploy

1. Send a message in a fresh Slack thread or DM; `aws ecs list-tasks --cluster <c>` shows one
   new task; router logs show the launch and the first result.
2. Send a second message in the same thread within 10 minutes; no new task appears and the
   reply comes back without the cold-start delay.
3. Wait 10 idle minutes; the task stops on its own (router logs nothing; `list-tasks` shows
   it gone). A third message launches a fresh task.
4. Cancel case: send two messages quickly; the first turn is abandoned, the same task serves
   the batched follow-up, no second task appears.

## 8. Rollback

Deploy the 0.11.2 router again; leave the 0.12.0 image in place. Warm workers already running
exit on their own within `worker_idle_secs`; the old router ignores them.
