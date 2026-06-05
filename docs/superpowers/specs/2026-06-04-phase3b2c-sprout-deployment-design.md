# Phase 3b-2c: the `sprout` deployment (replatform router + Fargate worker fleet)

**Date:** 2026-06-04
**Status:** Design approved, pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessors:** Phase 3b-1 (worker image + `Launcher` trait), 3b-2a (`S3StateStore`), 3b-2b (`FargateLauncher` + cloud worker config/secrets contract).
**Implementation repo:** `~/Github/sprout` (`git@github.com:root-global/sprout.git`, currently empty). This design doc lives with the other phase specs in cica; the CDK code lands in sprout.

## Goal

Stand up the real AWS deployment for the distributed architecture as **one CDK app** in `sprout`, and **replatform** cica off the current single-box setup in `root-infra`: a small always-on **router** (control plane) plus an ephemeral **Fargate worker fleet** (data plane), sharing durable state through an **S3 state bucket**. This enables the first real `RunTask` end-to-end and retires `root-infra`'s `RootAIStack`. Durable state on the existing EFS is preserved throughout.

## Context (the existing deployment, account `974767452524` / `eu-central-1`)

- cica runs today as a bare **EC2 `t3.medium`** (systemd, source-compiled) in the **default VPC** `vpc-0146f4edffb9ece24` (`172.31.0.0/16`, all-public subnets, no NAT), defined by `RootAIStack` in `root-infra` (CDK TypeScript, `aws-cdk-lib 2.189`, pnpm).
- Durable state lives on an **EFS** file system tagged `root-ai-data` (`RETAIN`), mounted at `/data`; `/data/cica/config.toml` holds all secrets in plaintext and the session/memory state.
- The instance role (`RootAIInstanceRole`) has only `AmazonSSMManagedInstanceCore` — **no S3, ECS, or Secrets Manager** access.
- An **RDS** instance exists; the cica box reaches it via SG-to-SG (the RDS SG `sg-06c0fc1ee54a5d6e8` allows ingress from the instance SG on 5432). DB-access skills use this path today.
- **No** S3 buckets, ECR repos, Secrets Manager secrets, or NAT gateways exist yet.
- cica's `install.sh` + CI build matrix already publish prebuilt release binaries; the `Dockerfile` (Ubuntu 24.04, bakes bun/cursor-cli/claude-code, `ENV XDG_CONFIG_HOME=/data`) is production-ready.

## Key decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| Ownership | sprout owns the **entire** deployment (router + fleet) as **one CDK app**; `root-infra`/`RootAIStack` retired at the end | Single source of truth, single `cdk destroy` cleanup; the deployment code never belonged in `root-infra`. |
| Router migration | **Replace** the instance (fresh, clean IaC) but **adopt the existing EFS** by filesystem-id | The instance is cattle, the EFS is the pet — all durable state is on EFS (`RETAIN`), so a fresh instance mounting it loses nothing and needs no re-init. |
| Router size | small **EC2** (`t3.small`) + systemd, default VPC, mounts EFS | The router no longer runs the agent/skills (those move to workers), so it's right-sized down; closest to what works today. |
| Worker network | **dedicated VPC** `10.20.0.0/16`, **private subnets**, **1 NAT gateway**, `assign_public_ip = false` | Isolates the arbitrary-code agent workload from the production network; no public IPs on workers; non-overlapping CIDR keeps peering/TGW open. |
| S3 reachability | **S3 gateway endpoint** (free) in the worker VPC | State traffic never traverses NAT. |
| RDS access | **VPC peering** worker-VPC ↔ default-VPC + an ingress rule on `sg-06c0fc1ee54a5d6e8` for `10.20.0.0/16:5432` | Workers keep private DB access; a Transit Gateway supersedes this when multiple DB VPCs appear (future). |
| Router↔worker comms | none over the network — router calls `ecs:RunTask` (control plane) and both exchange job/result/state via **S3** | Store-mediated dispatch (from 3a); no VPC-to-VPC path needed except the RDS peering. |
| Secrets | router reads its EFS `config.toml` (unchanged); a **new Secrets Manager secret** holds the AI key(s) → injected as `CICA_CURSOR_API_KEY`/`CICA_CLAUDE_API_KEY` env into the worker task-def | Secrets never in the image or S3; matches the 3b-2b env overlay. |
| Version control | one `cicaVersion` knob: workers via an immutable ECR image tag; router via `install.sh` pinned to that version | Reproducible, rollback-able, one version across both planes. |
| Cleanup | one `cdk destroy`; stateful resources (`EFS`, `S3` state bucket) are `RETAIN` and called out explicitly | Predictable teardown without data loss. |

## Architecture

```
                       AWS account 974767452524 / eu-central-1   —   one CDK app (sprout)

  DEFAULT VPC (172.31.0.0/16)                      DEDICATED WORKER VPC (10.20.0.0/16, sprout-created)
  ┌──────────────────────────────┐                 ┌─────────────────────────────────────────────┐
  │ Router: EC2 t3.small (systemd)│                 │ private subnet 1a / 1b   (no public IPs)      │
  │  cica: channels + dispatch +  │                 │   ┌────────┐  ┌────────┐  ephemeral Fargate   │
  │  memory index                 │                 │   │ worker │  │ worker │  cica-worker tasks    │
  │  mounts EFS root-ai-data /data│                 │   └────────┘  └────────┘  scale-to-zero        │
  │  RDS (5432) ◄────peering──────┼─────────────────┼─► (DB skills, via peering + SG ingress)        │
  └───────────┬───────────────────┘                 │   NAT gw → AI API     S3 gateway endpoint      │
              │ ecs:RunTask / DescribeTasks / Stop   └─────────────────────────────────────────────┘
              └───────────────────────────────────────────────►  starts a worker task

     job/result + sessions/memories  ───────────►   S3 STATE BUCKET   ◄───────────   worker pull/push
     AI key  ──── Secrets Manager ───► worker task env (CICA_*_API_KEY)
```

## Components (one CDK app; logical stacks/constructs)

The app is a single deployable unit. Internally it may be one stack or a few constructs/nested stacks, but `cdk deploy`/`cdk destroy` act on the whole app.

### 1. Networking (worker VPC)
- `ec2.Vpc` `10.20.0.0/16`, 2 AZs, **private-with-egress** subnets + minimal public subnets for the NAT, **`natGateways: 1`**.
- **S3 gateway endpoint**.
- **VPC peering** to the default VPC (`ec2.CfnVPCPeeringConnection`) + routes both directions for the RDS path; an ingress rule on the imported RDS SG `sg-06c0fc1ee54a5d6e8` for `10.20.0.0/16` on 5432. The default VPC and its RDS SG are imported by id.

### 2. Storage
- **Adopt EFS**: `efs.FileSystem.fromFileSystemAttributes({ fileSystemId: <root-ai-data id>, securityGroup })`. The router mounts it at `/data`. (Mount-target ownership handled in the cutover sequence — see below.)
- **S3 state bucket** (new): versioning off, SSE-S3, block-public, `removalPolicy: RETAIN` (flagged). Used for `turns/<id>/{job,result}`, `session/<id>`, and memories.

### 3. Secrets
- A **Secrets Manager secret** (e.g. `cica/worker/ai-keys`) holding `cursor_api_key` / `claude_api_key`. The worker task-def maps them to env `CICA_CURSOR_API_KEY` / `CICA_CLAUDE_API_KEY` via `secrets:` (ECS pulls them at task launch). Populated once by the operator.

### 4. ECR + worker image
- **ECR repo** `cica-worker`.
- A build+push script (`scripts/push-image.sh` or a pnpm script): builds the image from cica's `Dockerfile` at `cicaVersion`, layers a thin non-secret deployment `config.toml` (`backend`, `store = "s3"`, `[deployment.s3]` bucket/region) to `/data/cica/config.toml`, tags it `cica-worker:<cicaVersion>`, and pushes to ECR. (CI workflow with the `github-ci-role-infra` OIDC role is a later add.)

### 5. ECS cluster + task-def
- **ECS cluster** in the worker VPC.
- **Fargate task-def** `cica-worker`: the container named **`cica-worker`** from the ECR image at `cica-worker:<cicaVersion>`, CPU/memory sized for one turn (e.g. 1 vCPU / 2 GB, tunable), awsvpc networking, CloudWatch Logs, the `CICA_*_API_KEY` secrets. The launcher overrides the container command per turn to `worker --turn <id>`; the task-def's default command is irrelevant.
- **Task role**: S3 RW on the state bucket (later: RDS connect). **Execution role**: ECR pull + Logs + read the Secrets Manager secret.

### 6. Router (EC2)
- `ec2.Instance` `t3.small`, Ubuntu 24.04, default VPC, its own SG (egress-only; granted to the RDS SG for the router's own needs if any — though skills run on workers now). Mounts the adopted EFS at `/data`.
- **User-data**: install cica via `install.sh` pinned to `cicaVersion` (no on-box compile), install the systemd unit (`ExecStart=/usr/local/bin/cica`), start it. Config comes from the EFS `/data/cica/config.toml` (already present); we add `provider = "fargate"` + `store = "s3"` + `[deployment.fargate]` + `[deployment.s3]` during cutover.
- **Router IAM role**: `AmazonSSMManagedInstanceCore` + `ecs:RunTask`/`DescribeTasks`/`StopTask` (scoped to the cluster/task-def), `iam:PassRole` for the task + execution roles, and S3 RW on the state bucket.

### 7. Version-update mechanism (`cicaVersion`)
- CDK context/param `cicaVersion` (semver or git ref) is the single knob.
- **Worker update:** run the build+push script at the new version → `cdk deploy` (task-def revision now points at `cica-worker:<new>`). Next `RunTask` uses it. Rollback = `cdk deploy` with the prior version (image tag still in ECR).
- **Router update:** an SSM "update" command (re-run `install.sh @<new>` + `systemctl restart cica`) — instant, no instance replacement; or an instance refresh that re-runs user-data. Both planes pinned to the same `cicaVersion`.

## Cutover (explicitly sequenced, reversible)

1. **Deploy sprout** (`cdk deploy`): worker VPC + S3 bucket + ECR + image pushed + Secrets Manager (operator populates it) + ECS cluster/task-def + the new router instance (mounting EFS). The **old box keeps owning the channel tokens** at this point — the new router does not start its channel listeners yet (so the two don't double-consume Slack/Telegram).
2. **Validate the Fargate path first** — drive one turn through the dispatch path with `provider = fargate` + `store = s3` (a one-off invocation on the new router, or any host holding the router IAM — without starting channel listeners), and confirm a real worker task launches, runs, and the result round-trips through S3. This proves the fleet before any channel cutover.
3. **Channel cutover** — stop cica on the old box (Slack/Telegram allow one active consumer per token), set the new router's `config.toml` to `provider = "fargate"` + `store = "s3"` + the `[deployment.fargate]`/`[deployment.s3]` sections, restart. The new router now owns channels and dispatches to Fargate.
4. **Validate end-to-end** — a real channel message produces a Fargate-executed turn that replies; confirm `turns/<id>` round-trips and a follow-up resumes from S3-restored session state.
5. **Retire `root-infra`** — delete `RootAIStack`. The EFS is `RETAIN` (and now referenced by sprout), so it and its data survive; the old instance is destroyed.

**EFS mount-target note:** EFS allows one mount target per AZ per filesystem. During cutover the old stack's mount targets (in the default VPC) must be released before/at the time sprout's router creates its own, or sprout adopts the existing mount targets. The implementation plan sequences this (e.g. create sprout's router + mount targets in the same AZs after the old stack's targets are removed, or import the targets) to avoid an AZ conflict. Brief EFS unavailability is acceptable during the maintenance cutover.

**Rollback:** at any step before deleting `root-infra`, flip the new (or old) router's config back to `provider = "local"` (in-process, exactly today's behavior) and re-point channels to whichever box is healthy. The S3 bucket + Fargate fleet are inert when unused.

## Error handling / operational concerns

- **IAM least-privilege:** router `ecs:RunTask`/`PassRole` scoped to the cluster + the two task roles; task role S3 scoped to the state bucket ARN; execution role secret-read scoped to the one secret.
- **Worker can't pull image / no creds:** surfaces as a `RunTask` failure or a non-zero task exit → the `FargateLauncher` reports a turn error (3b-2b). CloudWatch Logs on the task-def capture worker stderr for debugging.
- **NAT/egress down:** worker can't reach the AI API → turn fails; the router's channel/cron retry path applies.
- **Existing sessions:** live on EFS (single-box `/data/cica`), not S3. After cutover, new turns use S3; old conversations don't auto-migrate (acceptable — a bulk push is a possible later nicety, not required).
- **Cost:** ~1 NAT gateway (~$32/mo) + the small router EC2 + per-turn Fargate (scale-to-zero) + S3/ECR storage. No idle worker cost.

## Testing / validation strategy

- **`cdk synth`** clean; review the synthesized template (IAM scoping, the RETAIN policies, the peering + SG ingress).
- **Image build** succeeds and pushes to ECR; the baked `/data/cica/config.toml` is non-secret only.
- **Acceptance test (the headline):** after deploy, a real turn dispatched with `provider = fargate` round-trips through a Fargate worker via S3 (step 2/4 above) — the first real `RunTask`. A follow-up message resumes session state restored from S3 into a fresh worker (the isolation proof, now on real Fargate).
- **DB-skill check:** a skill that queries RDS works from a worker (validates the peering + SG ingress).
- **Rollback drill:** flipping `provider = local` returns to single-box behavior.
- No unit-test harness for CDK beyond `cdk synth` + targeted assertions (`Template.fromStack`) on the critical pieces (IAM policies, removal policies, the task-def container name + secrets) if we add `aws-cdk-lib/assertions`.

## Out of scope (later)

- The router→RDS Transit Gateway for **multiple** DB VPCs (peering covers the single current RDS).
- A CI image-build/deploy pipeline in sprout (manual script first; OIDC `github-ci-role-infra` workflow later).
- Channel-token secrets in Secrets Manager (router keeps them in EFS config for now; only worker AI keys move to Secrets Manager).
- Bulk migration of existing EFS sessions into S3.
- ECR/Logs **interface** VPC endpoints (NAT covers ECR pulls; add endpoints only if NAT data cost warrants).
- Warm/reused workers; autoscaling beyond per-turn `RunTask`.
- Moving the router itself off the default VPC.
