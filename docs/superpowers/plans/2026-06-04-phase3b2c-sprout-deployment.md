# Phase 3b-2c: the `sprout` deployment CDK Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build, in `~/Github/sprout`, a single CDK app that deploys cica's distributed AWS deployment — a small always-on router (EC2 + adopted EFS) plus an ephemeral Fargate worker fleet sharing an S3 state bucket — and produce the cutover runbook that replatforms off `root-infra`/`RootAIStack`.

**Architecture:** One CDK TypeScript app with **two stacks**: `SproutFleetStack` (worker VPC + NAT + S3 endpoint + peering, S3 state bucket, Secrets Manager, ECR, ECS cluster + `cica-worker` task-def, worker IAM) deploys conflict-free while the old box runs; `SproutRouterStack` (EC2 `t3.small` in the default VPC, adopts the existing EFS by id, router IAM granting `ecs:RunTask`/`PassRole`/S3) deploys during the cutover window (after the old stack's EFS mount targets are released). The single `cicaVersion` knob drives both planes (workers via an immutable ECR tag, router via `install.sh`).

**Tech Stack:** CDK TypeScript, `aws-cdk-lib 2.189.1`, `constructs ^10`, `pnpm 9.15.4`, ts-node; `aws-cdk-lib/assertions` for `Template` tests. Account `974767452524`, region `eu-central-1`.

---

## Important context for the implementer

**This plan produces IaC + scripts + a runbook. The actual `cdk deploy` and the cutover happen on real AWS, run by the operator** — the headline "first real `RunTask`" acceptance test is operator-run (like the deferred RunTask in 3b-2b). Subagents write and `cdk synth`-verify the code; they do not deploy.

**Test gate per task = `pnpm cdk synth` (offline) + `aws-cdk-lib/assertions` `Template.fromStack` assertions** on the security-critical properties. To keep `synth` working **without AWS credentials**, import the existing (default) VPC by **explicit attributes**, not `Vpc.fromLookup` (which hits AWS at synth). The worker VPC is newly created (no lookup). Verified identifiers from the existing deployment:

- Account `974767452524`, region `eu-central-1`.
- Default VPC `vpc-0146f4edffb9ece24` (`172.31.0.0/16`). Public subnets: `subnet-0764b547b7f829c85` (1a), `subnet-0ae3e11055329804d` (1a), `subnet-0475bc6040f5d4996` (1b), `subnet-086c289f8b83398bf` (1c). Route tables seen: `rtb-0119be5b103b3d0ef`, `rtb-0793e18ac045ce5c0`.
- RDS security group `sg-06c0fc1ee54a5d6e8`.
- The EFS (created by `RootAIStack`, tagged `root-ai-data`, RETAIN). Its **filesystem id is a deploy-time value** — obtain with `aws cloudformation describe-stacks --stack-name RootAIStack --query "Stacks[0].Outputs[?OutputKey=='FileSystemId'].OutputValue" --output text` and pass via context `efsFileSystemId`.
- cica repo: `https://github.com/dcvz/cica.git`; `install.sh` + release binaries published by CI; `Dockerfile` at the repo root.

**Deterministic naming (so the worker image's baked config needs no deploy outputs):**
- S3 state bucket: **`cica-state-974767452524-eu-central-1`** (explicit `bucketName`).
- ECR repo: **`cica-worker`**. Image tag: **`cica-worker:<cicaVersion>`**.
- Secrets Manager secret: **`cica/worker/ai-keys`** (keys `cursor_api_key`, `claude_api_key`).
- ECS cluster: **`cica-workers`**. Task-def family: **`cica-worker`**. Container name: **`cica-worker`** (matches `[deployment.fargate].container_name`).

**aws-cdk-lib version note:** target `2.189.1`. A few low-level constructs below (`CfnVPCPeeringConnection`, `CfnRoute`, `efs.CfnMountTarget`, the ECS `Secret.fromSecretsManager` env mapping, `ec2.Vpc` subnet config) have props that occasionally shift between minor versions. After writing each, run `pnpm cdk synth` and fix any prop mismatch against the installed types, preserving behavior. Report deviations (same honest pattern as the Rust phases' SDK notes).

## File structure (`~/Github/sprout`)

```
sprout/
  package.json          # pnpm, aws-cdk-lib 2.189.1, ts-node — mirrors root-ai
  cdk.json              # app = ts-node bin/sprout.ts
  tsconfig.json         # mirrors root-ai
  .gitignore            # node_modules, cdk.out, *.js
  bin/sprout.ts         # the app: both stacks, env, cicaVersion context, dependency
  lib/
    fleet-stack.ts      # SproutFleetStack (worker VPC, S3, Secrets, ECR, ECS, IAM)
    router-stack.ts     # SproutRouterStack (EC2 + adopted EFS + router IAM)
  test/
    fleet-stack.test.ts # Template assertions
    router-stack.test.ts
  scripts/
    push-image.sh       # build cica-worker image @cicaVersion + push to ECR
    update-router.sh     # SSM doc / command to bump the router's cica version
  RUNBOOK.md            # the sequenced cutover + rollback
  README.md
```

---

### Task 1: Scaffold the sprout CDK project

**Files:** Create `package.json`, `cdk.json`, `tsconfig.json`, `.gitignore`, `bin/sprout.ts`, `lib/fleet-stack.ts` (empty stack), `README.md` in `~/Github/sprout`.

- [ ] **Step 1: Create the project files**

`~/Github/sprout/package.json`:
```json
{
  "name": "sprout",
  "version": "0.1.0",
  "bin": { "sprout": "bin/sprout.ts" },
  "scripts": {
    "build": "tsc",
    "cdk": "cdk",
    "test": "jest"
  },
  "devDependencies": {
    "@types/jest": "^29.5.0",
    "@types/node": "^22.7.9",
    "aws-cdk": "^2",
    "jest": "^29.7.0",
    "ts-jest": "^29.1.0",
    "ts-node": "^10.9.2",
    "typescript": "~5.6.3"
  },
  "dependencies": {
    "aws-cdk-lib": "2.189.1",
    "constructs": "^10.0.0"
  },
  "packageManager": "pnpm@9.15.4"
}
```

`~/Github/sprout/cdk.json`:
```json
{
  "app": "npx ts-node --prefer-ts-exts bin/sprout.ts",
  "watch": { "include": ["**"], "exclude": ["node_modules", "cdk.out"] },
  "context": {
    "@aws-cdk/core:checkSecretUsage": true,
    "@aws-cdk/core:target-partitions": ["aws"]
  }
}
```

`~/Github/sprout/tsconfig.json` (copy of root-ai's, plus jest types):
```json
{
  "compilerOptions": {
    "target": "ES2020",
    "module": "commonjs",
    "lib": ["ES2020"],
    "declaration": true,
    "strict": true,
    "noImplicitAny": true,
    "strictNullChecks": true,
    "noImplicitThis": true,
    "alwaysStrict": true,
    "noImplicitReturns": true,
    "inlineSourceMap": true,
    "inlineSources": true,
    "experimentalDecorators": true,
    "strictPropertyInitialization": false,
    "typeRoots": ["./node_modules/@types"]
  },
  "exclude": ["node_modules", "cdk.out"]
}
```

`~/Github/sprout/.gitignore`:
```
node_modules
cdk.out
*.js
*.d.ts
!jest.config.js
.cdk.staging
cdk.context.json
```

`~/Github/sprout/jest.config.js`:
```js
module.exports = {
  testEnvironment: "node",
  roots: ["<rootDir>/test"],
  testMatch: ["**/*.test.ts"],
  transform: { "^.+\\.tsx?$": "ts-jest" },
};
```

`~/Github/sprout/lib/fleet-stack.ts` (empty stack to start):
```ts
import * as cdk from "aws-cdk-lib";
import { Construct } from "constructs";

export class SproutFleetStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);
  }
}
```

`~/Github/sprout/bin/sprout.ts`:
```ts
#!/usr/bin/env node
import * as cdk from "aws-cdk-lib";
import { SproutFleetStack } from "../lib/fleet-stack";

const app = new cdk.App();

const account = process.env.CDK_DEFAULT_ACCOUNT || "974767452524";
const region = process.env.CDK_DEFAULT_REGION || "eu-central-1";
const env = { account, region };

new SproutFleetStack(app, "SproutFleetStack", { env });
```

`~/Github/sprout/README.md`: a short intro — "sprout: the cica distributed deployment (router + Fargate worker fleet). See RUNBOOK.md for deploy/cutover." plus `pnpm install`, `pnpm cdk synth`, `pnpm test`.

- [ ] **Step 2: Install + synth**

Run (in `~/Github/sprout`):
```
pnpm install
pnpm cdk synth
```
Expected: `pnpm install` succeeds; `cdk synth` prints an (essentially empty) `SproutFleetStack` template with no error.

- [ ] **Step 3: First commit**

```bash
cd ~/Github/sprout
git add -A
git commit -m "$(cat <<'EOF'
chore: scaffold sprout CDK app (TypeScript, pnpm, aws-cdk-lib 2.189)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```
(The remote `git@github.com:root-global/sprout.git` already exists; do not push until the operator says so.)

---

### Task 2: Fleet networking — worker VPC, NAT, S3 endpoint

**Files:** Modify `lib/fleet-stack.ts`; create `test/fleet-stack.test.ts`.

- [ ] **Step 1: Write the failing test**

`~/Github/sprout/test/fleet-stack.test.ts`:
```ts
import * as cdk from "aws-cdk-lib";
import { Template } from "aws-cdk-lib/assertions";
import { SproutFleetStack } from "../lib/fleet-stack";

function synth() {
  const app = new cdk.App();
  const stack = new SproutFleetStack(app, "SproutFleetStack", {
    env: { account: "974767452524", region: "eu-central-1" },
  });
  return Template.fromStack(stack);
}

test("creates the dedicated worker VPC 10.20.0.0/16", () => {
  const t = synth();
  t.hasResourceProperties("AWS::EC2::VPC", { CidrBlock: "10.20.0.0/16" });
});

test("creates exactly one NAT gateway", () => {
  const t = synth();
  t.resourceCountIs("AWS::EC2::NatGateway", 1);
});

test("creates an S3 gateway endpoint", () => {
  const t = synth();
  t.hasResourceProperties("AWS::EC2::VPCEndpoint", {
    VpcEndpointType: "Gateway",
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `pnpm test`
Expected: FAIL — no VPC/NAT/endpoint resources yet.

- [ ] **Step 3: Implement the worker VPC**

In `lib/fleet-stack.ts`, add the imports and VPC inside the constructor:
```ts
import * as cdk from "aws-cdk-lib";
import * as ec2 from "aws-cdk-lib/aws-ec2";
import { Construct } from "constructs";

export class SproutFleetStack extends cdk.Stack {
  public readonly vpc: ec2.Vpc;

  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    // Dedicated, isolated worker VPC. Non-overlapping CIDR (default VPC is
    // 172.31.0.0/16) keeps VPC peering / Transit Gateway open for future DBs.
    this.vpc = new ec2.Vpc(this, "WorkerVpc", {
      ipAddresses: ec2.IpAddresses.cidr("10.20.0.0/16"),
      maxAzs: 2,
      natGateways: 1, // one NAT for AI-API egress; workers have no public IP
      subnetConfiguration: [
        { name: "public", subnetType: ec2.SubnetType.PUBLIC, cidrMask: 24 },
        {
          name: "workers",
          subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
          cidrMask: 20,
        },
      ],
    });

    // S3 state traffic stays off the NAT.
    this.vpc.addGatewayEndpoint("S3Endpoint", {
      service: ec2.GatewayVpcEndpointAwsService.S3,
    });
  }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `pnpm test`
Expected: PASS (3 tests).
Run: `pnpm cdk synth` — succeeds offline (the new VPC needs no lookup).

- [ ] **Step 5: Commit**

```bash
git add lib/fleet-stack.ts test/fleet-stack.test.ts
git commit -m "feat(fleet): dedicated worker VPC (10.20.0.0/16, 1 NAT, S3 endpoint)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Fleet networking — peering to the default VPC + RDS access

**Files:** Modify `lib/fleet-stack.ts`, `test/fleet-stack.test.ts`.

> Workers reach the RDS (in the default VPC) over a VPC peering connection. We import the default VPC and RDS SG by id (no `fromLookup`, so synth stays offline).

- [ ] **Step 1: Add the failing test**

Append to `test/fleet-stack.test.ts`:
```ts
test("creates a VPC peering connection to the default VPC", () => {
  const t = synth();
  t.hasResourceProperties("AWS::EC2::VPCPeeringConnection", {
    PeerVpcId: "vpc-0146f4edffb9ece24",
  });
});

test("opens the RDS security group to the worker CIDR on 5432", () => {
  const t = synth();
  t.hasResourceProperties("AWS::EC2::SecurityGroupIngress", {
    GroupId: "sg-06c0fc1ee54a5d6e8",
    FromPort: 5432,
    ToPort: 5432,
    CidrIp: "10.20.0.0/16",
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `pnpm test` → the two new tests FAIL.

- [ ] **Step 3: Implement peering + routes + RDS ingress**

Add constants near the top of `lib/fleet-stack.ts`:
```ts
const DEFAULT_VPC_ID = "vpc-0146f4edffb9ece24";
const DEFAULT_VPC_CIDR = "172.31.0.0/16";
const RDS_SG_ID = "sg-06c0fc1ee54a5d6e8";
// Default-VPC route tables that need a return route to the worker VPC.
const DEFAULT_VPC_ROUTE_TABLE_IDS = [
  "rtb-0119be5b103b3d0ef",
  "rtb-0793e18ac045ce5c0",
];
```
At the end of the constructor:
```ts
    // --- Peering to the default VPC (for RDS) ---
    const peering = new ec2.CfnVPCPeeringConnection(this, "DefaultVpcPeering", {
      vpcId: this.vpc.vpcId,
      peerVpcId: DEFAULT_VPC_ID,
    });

    // Worker side: route default-VPC CIDR via the peering, from every worker subnet's RT.
    this.vpc.selectSubnets({
      subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
    }).subnets.forEach((subnet, i) => {
      new ec2.CfnRoute(this, `ToDefaultVpc${i}`, {
        routeTableId: subnet.routeTable.routeTableId,
        destinationCidrBlock: DEFAULT_VPC_CIDR,
        vpcPeeringConnectionId: peering.ref,
      });
    });

    // Default side: route the worker CIDR back via the peering.
    DEFAULT_VPC_ROUTE_TABLE_IDS.forEach((rtId, i) => {
      new ec2.CfnRoute(this, `FromDefaultVpc${i}`, {
        routeTableId: rtId,
        destinationCidrBlock: "10.20.0.0/16",
        vpcPeeringConnectionId: peering.ref,
      });
    });

    // Allow the worker CIDR into the RDS SG on Postgres.
    const rdsSg = ec2.SecurityGroup.fromSecurityGroupId(this, "RdsSg", RDS_SG_ID, {
      mutable: true,
    });
    rdsSg.addIngressRule(
      ec2.Peer.ipv4("10.20.0.0/16"),
      ec2.Port.tcp(5432),
      "cica workers to Postgres",
    );
```

- [ ] **Step 4: Run to verify pass + synth**

Run: `pnpm test` (all pass) and `pnpm cdk synth` (offline OK).
> Verify against 2.189: `subnet.routeTable.routeTableId` is the documented accessor; `CfnVPCPeeringConnection`/`CfnRoute` prop names (`vpcPeeringConnectionId`) match. The `addIngressRule` on an imported SG emits an `AWS::EC2::SecurityGroupIngress` with `GroupId` = the SG id — confirm the test matcher matches the synthesized shape; adjust the matcher if CDK emits `CidrIp` vs `CidrIpv4` differently.

- [ ] **Step 5: Commit**

```bash
git add lib/fleet-stack.ts test/fleet-stack.test.ts
git commit -m "feat(fleet): peer worker VPC to default VPC for RDS access

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Fleet — S3 state bucket, Secrets Manager, ECR

**Files:** Modify `lib/fleet-stack.ts`, `test/fleet-stack.test.ts`.

- [ ] **Step 1: Add failing tests**

```ts
test("creates the RETAIN, block-public S3 state bucket with the explicit name", () => {
  const t = synth();
  t.hasResourceProperties("AWS::S3::Bucket", {
    BucketName: "cica-state-974767452524-eu-central-1",
    PublicAccessBlockConfiguration: {
      BlockPublicAcls: true,
      BlockPublicPolicy: true,
      IgnorePublicAcls: true,
      RestrictPublicBuckets: true,
    },
  });
  t.hasResource("AWS::S3::Bucket", { DeletionPolicy: "Retain" });
});

test("creates the worker AI-keys secret and the ECR repo", () => {
  const t = synth();
  t.hasResourceProperties("AWS::SecretsManager::Secret", { Name: "cica/worker/ai-keys" });
  t.hasResourceProperties("AWS::ECR::Repository", { RepositoryName: "cica-worker" });
});
```

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement**

Add imports + resources to `lib/fleet-stack.ts`:
```ts
import * as s3 from "aws-cdk-lib/aws-s3";
import * as secretsmanager from "aws-cdk-lib/aws-secretsmanager";
import * as ecr from "aws-cdk-lib/aws-ecr";
```
Expose fields and create in the constructor:
```ts
  public readonly stateBucket: s3.Bucket;
  public readonly aiKeysSecret: secretsmanager.Secret;
  public readonly workerRepo: ecr.Repository;
```
```ts
    // Shared durable state (turns/<id>/{job,result}, session/<id>, memories).
    this.stateBucket = new s3.Bucket(this, "StateBucket", {
      bucketName: "cica-state-974767452524-eu-central-1",
      encryption: s3.BucketEncryption.S3_MANAGED,
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      removalPolicy: cdk.RemovalPolicy.RETAIN, // holds sessions/memories
    });

    // Worker AI credentials — injected as env into the task-def; operator fills the value.
    this.aiKeysSecret = new secretsmanager.Secret(this, "AiKeysSecret", {
      secretName: "cica/worker/ai-keys",
      description: "cica worker AI backend keys (cursor_api_key, claude_api_key)",
    });

    // Worker image registry.
    this.workerRepo = new ecr.Repository(this, "WorkerRepo", {
      repositoryName: "cica-worker",
      removalPolicy: cdk.RemovalPolicy.DESTROY, // images are reproducible
      emptyOnDelete: true,
    });
```

- [ ] **Step 4: Run → pass; `pnpm cdk synth`.**
> Verify: `emptyOnDelete` exists in 2.189 (older alias was `autoDeleteImages`); if the type errors, use the available prop. The `DeletionPolicy: Retain` assertion reads the CFN resource-level policy.

- [ ] **Step 5: Commit**

```bash
git add lib/fleet-stack.ts test/fleet-stack.test.ts
git commit -m "feat(fleet): S3 state bucket, AI-keys secret, worker ECR repo

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Fleet — ECS cluster, `cica-worker` task-def, task/execution roles

**Files:** Modify `lib/fleet-stack.ts`, `test/fleet-stack.test.ts`.

- [ ] **Step 1: Add failing tests**

```ts
test("creates the ECS cluster and a Fargate task-def with the cica-worker container", () => {
  const t = synth();
  t.hasResourceProperties("AWS::ECS::Cluster", { ClusterName: "cica-workers" });
  t.hasResourceProperties("AWS::ECS::TaskDefinition", {
    Family: "cica-worker",
    RequiresCompatibilities: ["FARGATE"],
    ContainerDefinitions: Match.arrayWith([
      Match.objectLike({ Name: "cica-worker" }),
    ]),
  });
});
```
(Add `import { Match } from "aws-cdk-lib/assertions";` to the test file.)

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement**

Add imports:
```ts
import * as ecs from "aws-cdk-lib/aws-ecs";
import * as iam from "aws-cdk-lib/aws-iam";
import * as logs from "aws-cdk-lib/aws-logs";
```
Expose fields + implement:
```ts
  public readonly cluster: ecs.Cluster;
  public readonly taskDef: ecs.FargateTaskDefinition;
```
```ts
    this.cluster = new ecs.Cluster(this, "WorkerCluster", {
      clusterName: "cica-workers",
      vpc: this.vpc,
    });

    // Execution role: pull ECR, write logs, read the AI-keys secret.
    const executionRole = new iam.Role(this, "WorkerExecRole", {
      assumedBy: new iam.ServicePrincipal("ecs-tasks.amazonaws.com"),
    });
    executionRole.addManagedPolicy(
      iam.ManagedPolicy.fromAwsManagedPolicyName(
        "service-role/AmazonECSTaskExecutionRolePolicy",
      ),
    );
    this.aiKeysSecret.grantRead(executionRole);

    // Task role: the running worker reads/writes the state bucket. (RDS connect
    // is network-level via the peering + SG; no IAM needed for Postgres.)
    const taskRole = new iam.Role(this, "WorkerTaskRole", {
      assumedBy: new iam.ServicePrincipal("ecs-tasks.amazonaws.com"),
    });
    this.stateBucket.grantReadWrite(taskRole);

    this.taskDef = new ecs.FargateTaskDefinition(this, "WorkerTaskDef", {
      family: "cica-worker",
      cpu: 1024,
      memoryLimitMiB: 2048,
      executionRole,
      taskRole,
    });

    this.taskDef.addContainer("cica-worker", {
      containerName: "cica-worker",
      image: ecs.ContainerImage.fromEcrRepository(this.workerRepo, cicaVersion(this)),
      logging: ecs.LogDrivers.awsLogs({
        streamPrefix: "cica-worker",
        logRetention: logs.RetentionDays.TWO_WEEKS,
      }),
      secrets: {
        CICA_CURSOR_API_KEY: ecs.Secret.fromSecretsManager(this.aiKeysSecret, "cursor_api_key"),
        CICA_CLAUDE_API_KEY: ecs.Secret.fromSecretsManager(this.aiKeysSecret, "claude_api_key"),
      },
      // Default command; the FargateLauncher overrides it per turn to
      // ["worker", "--turn", "<id>"]. Provide a harmless default.
      command: ["--help"],
    });
```
Add a `cicaVersion` helper at the bottom of the file (reads context, defaults):
```ts
function cicaVersion(scope: Construct): string {
  return (scope.node.tryGetContext("cicaVersion") as string) || "latest";
}
```

- [ ] **Step 4: Run → pass; `pnpm cdk synth`.**
> Verify against 2.189: `ecs.Secret.fromSecretsManager(secret, "jsonField")` is the documented JSON-key form; `LogDrivers.awsLogs({ logRetention })` prop name; `ContainerImage.fromEcrRepository(repo, tag)`. Adjust if the installed types differ. The task-def references `cica-worker:<cicaVersion>` — the image need not exist at synth/deploy time (only at RunTask).

- [ ] **Step 5: Commit**

```bash
git add lib/fleet-stack.ts test/fleet-stack.test.ts
git commit -m "feat(fleet): ECS cluster + cica-worker Fargate task-def + IAM roles

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Worker image build + push script

**Files:** Create `scripts/push-image.sh`.

> Builds the `cica-worker` image from cica's `Dockerfile` at `cicaVersion`, layers a non-secret deployment `config.toml`, tags `cica-worker:<cicaVersion>`, pushes to ECR. No secrets in the image.

- [ ] **Step 1: Write the script**

`~/Github/sprout/scripts/push-image.sh`:
```bash
#!/usr/bin/env bash
set -euo pipefail

# Build + push the cica-worker image for a given cica version.
# Usage: CICA_VERSION=v0.8.0 ./scripts/push-image.sh
#   CICA_VERSION  - git ref/tag of cica to build (required)
#   AWS_REGION    - default eu-central-1
#   ACCOUNT_ID    - default 974767452524

CICA_VERSION="${CICA_VERSION:?set CICA_VERSION to a cica git ref/tag}"
AWS_REGION="${AWS_REGION:-eu-central-1}"
ACCOUNT_ID="${ACCOUNT_ID:-974767452524}"
REPO="cica-worker"
REGISTRY="${ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"
IMAGE="${REGISTRY}/${REPO}:${CICA_VERSION}"
BUCKET="cica-state-${ACCOUNT_ID}-${AWS_REGION}"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

echo "==> Cloning cica @ ${CICA_VERSION}"
git clone --depth 1 --branch "${CICA_VERSION}" https://github.com/dcvz/cica.git "$workdir/cica"

echo "==> Building base worker image from cica Dockerfile"
docker build -t "cica-base:${CICA_VERSION}" "$workdir/cica"

echo "==> Layering non-secret deployment config.toml"
cat > "$workdir/config.toml" <<TOML
backend = "cursor"

[deployment]
store = "s3"
provider = "local"

[deployment.s3]
bucket = "${BUCKET}"
region = "${AWS_REGION}"
TOML
cat > "$workdir/Dockerfile.deploy" <<DOCKER
FROM cica-base:${CICA_VERSION}
COPY config.toml /data/cica/config.toml
DOCKER
docker build -f "$workdir/Dockerfile.deploy" -t "$IMAGE" "$workdir"

echo "==> Logging in + pushing to ECR"
aws ecr get-login-password --region "$AWS_REGION" \
  | docker login --username AWS --password-stdin "$REGISTRY"
docker push "$IMAGE"

echo "==> Pushed ${IMAGE}"
```
Notes baked in:
- The worker's `config.toml` is **non-secret** (`backend`, `store = "s3"`, bucket/region). `provider = "local"` here is correct for the worker: inside the worker the turn runs **in-process** (the worker IS the executor; only the router uses `provider = "fargate"`). The AI key arrives via the `CICA_*_API_KEY` env from Secrets Manager.
- `backend = "cursor"` matches the production backend; if Claude is wanted, change it (or make it a script env var).

- [ ] **Step 2: Verify the script**

Run: `bash -n scripts/push-image.sh` (syntax check) → no output = OK.
Run: `chmod +x scripts/push-image.sh`.
If Docker + network are available, a dry run against a throwaway tag is optional; otherwise the operator runs it during deploy. (It needs Docker, the cica repo to be tagged at `CICA_VERSION`, and AWS creds — so it is operator-run, not part of synth.)

- [ ] **Step 3: Commit**

```bash
git add scripts/push-image.sh
git commit -m "feat(scripts): build + push cica-worker image (non-secret config baked)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: `SproutRouterStack` — EC2 router + adopted EFS + router IAM

**Files:** Create `lib/router-stack.ts`, `test/router-stack.test.ts`; modify `bin/sprout.ts`.

> Deployed during the cutover window (after `RootAIStack`'s EFS mount targets are released — see RUNBOOK). Imports the default VPC + EFS by id (offline-synth-safe), grants the router `ecs:RunTask`/`PassRole`/S3.

- [ ] **Step 1: Write the failing test**

`~/Github/sprout/test/router-stack.test.ts`:
```ts
import * as cdk from "aws-cdk-lib";
import { Template, Match } from "aws-cdk-lib/assertions";
import { SproutFleetStack } from "../lib/fleet-stack";
import { SproutRouterStack } from "../lib/router-stack";

function synth() {
  const app = new cdk.App({ context: { efsFileSystemId: "fs-0123456789abcdef0" } });
  const env = { account: "974767452524", region: "eu-central-1" };
  const fleet = new SproutFleetStack(app, "SproutFleetStack", { env });
  const router = new SproutRouterStack(app, "SproutRouterStack", { env, fleet });
  return Template.fromStack(router);
}

test("creates a t3.small router instance", () => {
  synth().hasResourceProperties("AWS::EC2::Instance", { InstanceType: "t3.small" });
});

test("creates EFS mount targets for the adopted filesystem", () => {
  synth().hasResourceProperties("AWS::EFS::MountTarget", {
    FileSystemId: "fs-0123456789abcdef0",
  });
});

test("router role can RunTask and PassRole", () => {
  const t = synth();
  t.hasResourceProperties("AWS::IAM::Policy", {
    PolicyDocument: Match.objectLike({
      Statement: Match.arrayWith([
        Match.objectLike({ Action: Match.arrayWith(["ecs:RunTask"]) }),
      ]),
    }),
  });
});
```

- [ ] **Step 2: Run → fail** (no router stack).

- [ ] **Step 3: Implement `lib/router-stack.ts`**

```ts
import * as cdk from "aws-cdk-lib";
import * as ec2 from "aws-cdk-lib/aws-ec2";
import * as efs from "aws-cdk-lib/aws-efs";
import * as iam from "aws-cdk-lib/aws-iam";
import { Construct } from "constructs";
import { SproutFleetStack } from "./fleet-stack";

const DEFAULT_VPC_ID = "vpc-0146f4edffb9ece24";
const DEFAULT_VPC_CIDR = "172.31.0.0/16";
const DEFAULT_VPC_AZS = ["eu-central-1a", "eu-central-1b", "eu-central-1c"];
const DEFAULT_PUBLIC_SUBNETS = [
  "subnet-0764b547b7f829c85",
  "subnet-0475bc6040f5d4996",
  "subnet-086c289f8b83398bf",
];

export interface SproutRouterStackProps extends cdk.StackProps {
  fleet: SproutFleetStack;
}

export class SproutRouterStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props: SproutRouterStackProps) {
    super(scope, id, props);

    const cicaVersion = (this.node.tryGetContext("cicaVersion") as string) || "main";
    const efsId = this.node.tryGetContext("efsFileSystemId") as string;
    if (!efsId) {
      throw new Error("context efsFileSystemId is required (the adopted root-ai-data EFS)");
    }

    // Import the default VPC by attributes (offline-synth-safe; no fromLookup).
    const vpc = ec2.Vpc.fromVpcAttributes(this, "DefaultVpc", {
      vpcId: DEFAULT_VPC_ID,
      availabilityZones: DEFAULT_VPC_AZS,
      vpcCidrBlock: DEFAULT_VPC_CIDR,
      publicSubnetIds: DEFAULT_PUBLIC_SUBNETS,
    });

    const sg = new ec2.SecurityGroup(this, "RouterSg", {
      vpc,
      description: "cica router",
      allowAllOutbound: true,
    });

    // Fresh EFS mount targets for the adopted filesystem (the old stack's were
    // released on RootAIStack teardown — see RUNBOOK). One per AZ subnet.
    DEFAULT_PUBLIC_SUBNETS.forEach((subnetId, i) => {
      new efs.CfnMountTarget(this, `EfsMount${i}`, {
        fileSystemId: efsId,
        subnetId,
        securityGroups: [sg.securityGroupId],
      });
    });

    // Router IAM: SSM + dispatch to Fargate + S3 state.
    const role = new iam.Role(this, "RouterRole", {
      assumedBy: new iam.ServicePrincipal("ec2.amazonaws.com"),
    });
    role.addManagedPolicy(
      iam.ManagedPolicy.fromAwsManagedPolicyName("AmazonSSMManagedInstanceCore"),
    );
    role.addToPolicy(new iam.PolicyStatement({
      actions: ["ecs:RunTask", "ecs:DescribeTasks", "ecs:StopTask"],
      resources: [
        props.fleet.taskDef.taskDefinitionArn,
        `arn:aws:ecs:${this.region}:${this.account}:task/cica-workers/*`,
      ],
    }));
    role.addToPolicy(new iam.PolicyStatement({
      actions: ["iam:PassRole"],
      resources: [
        props.fleet.taskDef.taskRole.roleArn,
        props.fleet.taskDef.executionRole!.roleArn,
      ],
    }));
    props.fleet.stateBucket.grantReadWrite(role);

    const userData = ec2.UserData.forLinux();
    userData.addCommands(
      "apt-get update",
      "DEBIAN_FRONTEND=noninteractive apt-get install -y nfs-common curl",
      "mkdir -p /data",
      `mount -t nfs4 -o nfsvers=4.1,rsize=1048576,wsize=1048576,hard,timeo=600,retrans=2,noresvport ${efsId}.efs.${this.region}.amazonaws.com:/ /data`,
      `echo "${efsId}.efs.${this.region}.amazonaws.com:/ /data nfs4 nfsvers=4.1,rsize=1048576,wsize=1048576,hard,timeo=600,retrans=2,noresvport,_netdev 0 0" >> /etc/fstab`,
      "mkdir -p /data/cica /home/ubuntu/.config",
      "ln -sf /data/cica /home/ubuntu/.config/cica",
      "chown -R ubuntu:ubuntu /data/cica /home/ubuntu/.config",
      // Install cica from the prebuilt release (no on-box compile).
      `sudo -u ubuntu bash -c 'curl -fsSL https://raw.githubusercontent.com/dcvz/cica/${cicaVersion}/install.sh | CICA_VERSION=${cicaVersion} sh'`,
      "ln -sf /home/ubuntu/.local/bin/cica /usr/local/bin/cica || true",
      `cat > /etc/systemd/system/cica.service << 'SVCFILE'
[Unit]
Description=cica router
After=network.target

[Service]
Type=simple
User=ubuntu
ExecStart=/usr/local/bin/cica
Restart=always
RestartSec=10
Environment=HOME=/home/ubuntu

[Install]
WantedBy=multi-user.target
SVCFILE`,
      "systemctl daemon-reload",
      "systemctl enable cica.service",
      // NOT started here — operator flips config to provider=fargate first (RUNBOOK).
    );

    const instance = new ec2.Instance(this, "RouterInstance", {
      vpc,
      vpcSubnets: { subnets: [ec2.Subnet.fromSubnetId(this, "RouterSubnet", DEFAULT_PUBLIC_SUBNETS[0])] },
      instanceType: ec2.InstanceType.of(ec2.InstanceClass.T3, ec2.InstanceSize.SMALL),
      machineImage: ec2.MachineImage.fromSsmParameter(
        "/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id",
      ),
      securityGroup: sg,
      role,
      userData,
      blockDevices: [{
        deviceName: "/dev/sda1",
        volume: ec2.BlockDeviceVolume.ebs(10, {
          volumeType: ec2.EbsDeviceVolumeType.GP3,
          encrypted: true,
          deleteOnTermination: true,
        }),
      }],
    });
    cdk.Tags.of(instance).add("Name", "cica-router");

    new cdk.CfnOutput(this, "RouterInstanceId", { value: instance.instanceId });
    new cdk.CfnOutput(this, "SSMConnect", {
      value: `aws ssm start-session --target ${instance.instanceId}`,
    });
  }
}
```
Wire it into `bin/sprout.ts`:
```ts
import { SproutRouterStack } from "../lib/router-stack";
// ...
const fleet = new SproutFleetStack(app, "SproutFleetStack", { env });
const router = new SproutRouterStack(app, "SproutRouterStack", { env, fleet });
router.addDependency(fleet);
```

- [ ] **Step 4: Run → pass; `pnpm cdk synth`.**
> Verify against 2.189: `efs.CfnMountTarget` props; `ec2.Subnet.fromSubnetId` for `vpcSubnets`; `taskDef.executionRole` is `IRole | undefined` (the `!` assumes we always set it — we do). Confirm the `install.sh` invocation matches cica's actual install contract (env var name / URL); if `install.sh` resolves the binary to a different path than `~/.local/bin/cica`, fix the symlink line. Report what the real install.sh expects.

- [ ] **Step 5: Commit**

```bash
git add lib/router-stack.ts test/router-stack.test.ts bin/sprout.ts
git commit -m "feat(router): t3.small router stack adopting the EFS + dispatch IAM

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Deploy/update scripts + the cutover RUNBOOK

**Files:** Create `scripts/update-router.sh`, `RUNBOOK.md`; update `README.md`.

- [ ] **Step 1: Router-update helper**

`~/Github/sprout/scripts/update-router.sh`:
```bash
#!/usr/bin/env bash
set -euo pipefail
# Bump the router's cica version in place via SSM (no instance churn).
# Usage: INSTANCE_ID=i-xxxx CICA_VERSION=v0.8.0 ./scripts/update-router.sh
INSTANCE_ID="${INSTANCE_ID:?set INSTANCE_ID}"
CICA_VERSION="${CICA_VERSION:?set CICA_VERSION}"
AWS_REGION="${AWS_REGION:-eu-central-1}"

aws ssm send-command \
  --region "$AWS_REGION" \
  --instance-ids "$INSTANCE_ID" \
  --document-name "AWS-RunShellScript" \
  --comment "update cica to ${CICA_VERSION}" \
  --parameters commands="[\
\"sudo -u ubuntu bash -c 'curl -fsSL https://raw.githubusercontent.com/dcvz/cica/${CICA_VERSION}/install.sh | CICA_VERSION=${CICA_VERSION} sh'\",\
\"systemctl restart cica\"]"
echo "Update command sent. Track with: aws ssm list-command-invocations --instance-id ${INSTANCE_ID} --details"
```
`bash -n scripts/update-router.sh` → OK; `chmod +x`.

- [ ] **Step 2: Write `RUNBOOK.md`**

`~/Github/sprout/RUNBOOK.md` — the exact sequenced cutover + rollback (from the spec). Include:
```markdown
# sprout deploy + cutover runbook

Account 974767452524 / eu-central-1. Single CDK app, two stacks.

## 0. Prereqs
- `pnpm install`; AWS creds for the account; Docker (for the image).
- Find the EFS id: `aws cloudformation describe-stacks --stack-name RootAIStack \
    --query "Stacks[0].Outputs[?OutputKey=='FileSystemId'].OutputValue" --output text`
- Pick the cica version to run, e.g. `export CICA_VERSION=v0.8.0`.

## 1. Deploy the fleet (no conflict with the running box)
- `pnpm cdk deploy SproutFleetStack -c cicaVersion=$CICA_VERSION`
- Populate the secret (once):
  `aws secretsmanager put-secret-value --secret-id cica/worker/ai-keys \
     --secret-string '{"cursor_api_key":"…","claude_api_key":"…"}'`
- Build + push the worker image:
  `CICA_VERSION=$CICA_VERSION ./scripts/push-image.sh`

## 2. Validate the Fargate path BEFORE touching channels
- On any host with the router IAM (or after step 4's router, before starting channels),
  run a one-off turn with `provider = fargate` + `store = s3` and confirm a worker task
  launches (ECS console / `aws ecs list-tasks --cluster cica-workers`) and the result
  round-trips in S3 (`aws s3 ls s3://cica-state-974767452524-eu-central-1/turns/`).

## 3. Cutover (brief maintenance window — EFS mount targets move)
- Stop cica on the old box: `aws ssm start-session --target <old-id>` → `sudo systemctl stop cica`.
- Release the old EFS mount targets by deleting the old stack:
  `cd ~/Github/root-infra/root-ai && pnpm cdk destroy RootAIStack`
  (EFS is RETAIN → it and its data survive; the old instance + mount targets are removed.)
- Deploy the router (creates fresh mount targets on the same EFS + the new instance):
  `cd ~/Github/sprout && pnpm cdk deploy SproutRouterStack -c cicaVersion=$CICA_VERSION -c efsFileSystemId=<fs-id>`

## 4. Reconfigure + start the new router
- `aws ssm start-session --target <new-router-id>`; edit `/data/cica/config.toml`:
  add `provider = "fargate"`, `store = "s3"`, the `[deployment.s3]` and `[deployment.fargate]`
  sections (cluster `cica-workers`, task_definition `cica-worker`, the worker private subnet ids
  from `SproutFleetStack` outputs, the worker SG, `assign_public_ip = false`).
- `sudo systemctl start cica`.

## 5. Validate end-to-end
- Send a real channel message → confirm a Fargate task runs and the reply arrives;
  a follow-up resumes from S3-restored session state.
- DB-skill check: a skill that queries RDS works from a worker.

## Rollback (any time before deleting RootAIStack / or after)
- Set the router config back to `provider = "local"` and `systemctl restart cica` →
  in-process behavior, exactly as before. The fleet + bucket are inert when unused.

## Teardown
- `pnpm cdk destroy SproutRouterStack SproutFleetStack` removes everything except the
  RETAIN EFS and the RETAIN S3 state bucket (delete those by hand if intended).
```
Add the worker `[deployment.fargate]`/`[deployment.s3]` snippet to the runbook for copy-paste, using the fleet stack's outputs (add `CfnOutput`s for the worker subnet ids + SG id in Task 5 if not already present — add them now if missing).

- [ ] **Step 3: Add the fleet outputs the runbook references**

In `lib/fleet-stack.ts`, append outputs the operator needs for the router config:
```ts
    new cdk.CfnOutput(this, "ClusterName", { value: this.cluster.clusterName });
    new cdk.CfnOutput(this, "TaskDefArn", { value: this.taskDef.taskDefinitionArn });
    new cdk.CfnOutput(this, "StateBucketName", { value: this.stateBucket.bucketName });
    new cdk.CfnOutput(this, "WorkerSubnetIds", {
      value: this.vpc.selectSubnets({ subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS })
        .subnetIds.join(","),
    });
    new cdk.CfnOutput(this, "WorkerVpcId", { value: this.vpc.vpcId });
```
Run `pnpm cdk synth` → still clean. (No new test needed; these are outputs.)

- [ ] **Step 4: Update README + final gates**

Update `README.md` with the deploy/cutover summary pointing at `RUNBOOK.md`, the `cicaVersion` knob, and `pnpm test` / `pnpm cdk synth`.
Run: `pnpm test` (all green) and `pnpm cdk synth` (both stacks synth; `SproutRouterStack` needs `-c efsFileSystemId=fs-test` or the throwaway context — note the synth command in README).

- [ ] **Step 5: Commit**

```bash
git add scripts/update-router.sh RUNBOOK.md README.md lib/fleet-stack.ts
git commit -m "docs+scripts: cutover RUNBOOK, router-update helper, fleet outputs

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- One CDK app, two stacks, single `cdk destroy` → Tasks 1, 7, 8 (RUNBOOK teardown).
- Dedicated worker VPC `10.20.0.0/16`, private subnets, 1 NAT, `assign_public_ip=false`, S3 gateway endpoint → Task 2 (assign_public_ip is set in the router's `[deployment.fargate]` config per the RUNBOOK, Task 8).
- VPC peering + RDS SG ingress on 5432 → Task 3.
- Adopt EFS by id; S3 state bucket (RETAIN, explicit name) → Tasks 4 (bucket), 7 (EFS).
- Secrets Manager AI keys → worker env `CICA_*_API_KEY` → Tasks 4, 5.
- ECR + image build/push with baked non-secret config → Tasks 4, 6.
- ECS cluster + `cica-worker` task-def (container name, command override default, awsvpc, logs) + task/exec roles → Task 5.
- Router EC2 (t3.small, EFS mount, install.sh @cicaVersion) + router IAM (`ecs:RunTask`/`DescribeTasks`/`StopTask` + `PassRole` + S3) → Task 7.
- `cicaVersion` knob (worker ECR tag + router install.sh) + router-update SSM helper → Tasks 5, 6, 7, 8.
- Cutover sequence (incl. the EFS mount-target AZ-conflict resolution: destroy RootAIStack → deploy router) + rollback → Task 8 RUNBOOK.
- Testing = `cdk synth` + Template assertions; real RunTask is operator-run → noted up front + Task 8 validation steps.

**Placeholder scan:** No "TBD"/"handle appropriately". The aws-cdk-lib `> Verify against 2.189` notes (Tasks 3,4,5,7) are explicit "check the prop shape against the installed types and adjust, preserving behavior" guidance for version-sensitive low-level constructs — the same honest pattern as the Rust phases' SDK notes. The EFS filesystem id and the exact `install.sh` contract are real deploy-time values the implementer resolves (with the exact command to obtain them) — not undefined logic. The worker subnet/SG ids in the router config are emitted as stack outputs (Task 8) rather than hardcoded.

**Type consistency:** `SproutFleetStack` exposes `vpc`, `stateBucket`, `aiKeysSecret`, `workerRepo`, `cluster`, `taskDef` — consumed by name in `SproutRouterStack` (Task 7: `fleet.taskDef`, `fleet.stateBucket`) and the bin wiring. Names match across tasks: bucket `cica-state-974767452524-eu-central-1`, secret `cica/worker/ai-keys`, repo/family/container/cluster `cica-worker`/`cica-workers`, container env `CICA_CURSOR_API_KEY`/`CICA_CLAUDE_API_KEY` (matching cica's 3b-2b overlay), `container_name = "cica-worker"` (matching `[deployment.fargate]`). `cicaVersion` context key is read identically in the fleet helper, the router stack, and both scripts.

## Next (after this merges + cutover validates)

- The first real `RunTask` end-to-end is the acceptance test (operator-run, RUNBOOK §2/§5) — this is the deferred validation from 3b-2a/2b finally exercised on real Fargate, including the 3b-2b env-overlay path with a real backend.
- Then: retire `root-infra`/`RootAIStack` (RUNBOOK §3 already does this as part of cutover).
- Future (out of scope, tracked in the spec): Transit Gateway for multi-VPC DB access; a CI image-build/deploy workflow (OIDC `github-ci-role-infra`); channel-token secrets into Secrets Manager; ECR/Logs interface endpoints; GCP (3b-3, `CloudRunLauncher` + `GcsStateStore`).
