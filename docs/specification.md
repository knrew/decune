# decune v0.1 共有仕様

この文書は、`decune` v0.1 の利用者・貢献者向け共有仕様である。実装作業ログや milestone 履歴ではなく、公開挙動、設定形式、セキュリティ境界、検証方針を記録する。

## 目的

`decune` は、Dev Containers Specification の devcontainer を Rust 製の単一 CLI から起動、接続、停止、削除するためのツールである。VS Code や Node.js ベースの Dev Container CLI に依存しない。

v0.1 は Dev Container の次の 3 構成を正式対象にする。

1. image-based: `image`
2. Dockerfile-based: `build.dockerfile`
3. Docker Compose-based: `dockerComposeFile` + `service`

個人設定と project 設定は TOML で重ねられる。VS Code Dev Containers が暗黙に提供する Git/GitHub 認証、dotfiles、port forwarding、UID/GID sync も decune の責務として明示的に扱う。

## 参照仕様

- Development Containers Specification: <https://containers.dev/implementors/spec/>
- Dev Container metadata reference: <https://containers.dev/implementors/json_reference/>
- Dev Container Features reference: <https://containers.dev/implementors/features/>
- Dev Container CLI reference implementation: <https://github.com/devcontainers/cli>
- VS Code Dev Containers: <https://code.visualstudio.com/docs/devcontainers/containers>
- Docker CLI reference: <https://docs.docker.com/reference/cli/docker/>
- Docker Compose CLI reference: <https://docs.docker.com/reference/cli/docker/compose/>
- Docker Compose file reference: <https://docs.docker.com/reference/compose-file/>
- Docker Compose Specification: <https://github.com/compose-spec/compose-spec>
- Docker build context and `.dockerignore`: <https://docs.docker.com/build/concepts/context/>
- Docker bind mounts: <https://docs.docker.com/engine/storage/bind-mounts/>
- Docker container publish: <https://docs.docker.com/reference/cli/docker/container/run/>

## v0.1 の基本方針

### 実装対象

- Rust 製単一バイナリの CLI。
- Docker image / container / exec / copy / inspect 操作を `docker` CLI adapter 経由で行う。
- Docker Compose 操作を `docker compose` v2 CLI adapter 経由で行う。
- `bollard` crate への依存を廃止する。Docker Engine API を Rust 型で直接操作する実装は v0.1 の前提にしない。
- Dev Container の image-based / Dockerfile-based / Docker Compose-based 構成。
- JSONC としての `devcontainer.json` 読み込み。
- TOML による global/project 設定。
- Dev Container Features の OCI registry 取得、digest lock、local Feature、インストール、metadata merge。
- Docker Compose mode では、Feature、dotfiles、credentials、lifecycle、remote shell、port forwarding を primary service に適用する。
- Git HTTPS credential helper、SSH agent、GitHub CLI token forwarding。
- manual port forwarding と automatic port forwarding。
- Linux host での `updateRemoteUserUID` による UID/GID sync。
- lifecycle command と decune 固有 hooks。
- `up`、`rebuild`、`down`、`clean` サブコマンド。
- GitHub Releases の prebuilt archive による公式配布。

### Docker Compose 完全サポートの定義

v0.1 における「Docker Compose 完全サポート」とは、Dev Containers Specification が定義する Docker Compose-based 構成を、image/Dockerfile 構成と同じ decune 機能群で扱えることを指す。

具体的には以下を満たす。

- `dockerComposeFile` は string と string array の両方を受け付け、配列順を保持して Compose に渡す。
- `service` を primary service として扱い、remote shell、lifecycle、Feature、dotfiles、credentials、UID/GID sync、automatic forwarding の既定対象にする。
- `runServices` を受け付ける。未指定時は Compose project の全 service を起動対象にする。指定時も primary service は必ず起動対象に含める。
- Compose YAML の merge、include、profiles、anchors、extension fields、environment interpolation、relative path resolution、build semantics、network/volume/config/secret semantics は decune が再実装せず、Docker Compose v2 CLI に委譲する。
- decune は `docker compose config --format json` で正規化済み Compose model を取得し、検証、hash、対象 service/container 解決に使う。
- `forwardPorts` の `"service:port"` 形式を Compose service 名として扱い、primary service 以外の明示 forwarding に対応する。
- Compose が作成した resource の lifecycle は Compose project 単位で管理する。decune は Compose project name を明示指定し、他 project を拾わない。

「完全サポート」は、Compose Specification の全属性を decune が自前で解釈することを意味しない。Compose 仕様の追随は Docker Compose CLI に委譲し、decune は Dev Container と decune 固有機能を Compose project に安全に重ねる責務を持つ。

### 対象外

- 旧 `docker-compose` v1 standalone binary の公式対応。v0.1 は `docker compose` v2 plugin を必須にする。
- Kubernetes、Swarm stack、Docker Desktop UI、cloud provider 固有 orchestrator の直接サポート。
- Compose file を `dockerComposeFile` から git URL / OCI artifact / stdin で参照する構成。Dev Container の `dockerComposeFile` は `devcontainer.json` からの local path として扱う。
- primary service の replica/scale が 2 以上の構成。remote shell と lifecycle の対象 container が一意に決まらないため error にする。
- VS Code 拡張機能のインストールや `customizations.vscode` の適用。
- GPG agent forwarding。
- コンテナから任意の host command を実行する API。
- Windows host 向け公式配布。
- `cargo install` / `cargo install --git` を公式インストール手段として扱うこと。

## 必要な host tool

- Linux または macOS host。
- Docker CLI `docker`。
- Docker Compose v2 plugin。`docker compose version` が成功し、以下の capability があること。
  - `docker compose config --format json`
  - `docker compose ps --format json`
  - `docker compose build --with-dependencies`
  - `docker compose pull --policy always`
  - `docker compose pull --ignore-buildable`
  - `docker compose pull --include-deps`
  - `docker compose up --force-recreate`
  - `docker compose up --remove-orphans`
- Docker daemon へ接続できる権限。
- Git 認証連携を使う場合: host 側の `git`、必要に応じて `SSH_AUTH_SOCK`。
- GitHub CLI 連携を使う場合: host 側の `gh` と `gh auth token` が成功する状態。

Docker endpoint、context、credential helper、BuildKit、Compose profiles などは Docker CLI / Docker Compose CLI の標準挙動を継承する。decune は `DOCKER_HOST`、`DOCKER_CONTEXT`、`DOCKER_CONFIG`、`COMPOSE_PROFILES` などの host 環境変数を原則としてそのまま子 process に渡す。ただし secret value を log、state、hash、label、image layer に保存してはならない。

## 配布仕様

公式配布は GitHub Releases の prebuilt archive とする。release archive は以下を含む。

- `decune` binary
- `LICENSE`
- `README.md`

release asset:

- `decune-v{version}-{host_triple}.tar.gz`
- `SHA256SUMS`
- `release-manifest.json`

初期 target:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

container-side tools は release build 時に host binary へ埋め込む。Git repository には生成済み binary artifact を入れない。

container-side tool platform:

- `linux-amd64`
- `linux-arm64`

開発・debug 用 override として `DECUNE_CONTAINER_TOOLS_DIR` を残す。build-time の bundle 制御は `DECUNE_CONTAINER_TOOLS_BUNDLE` と `DECUNE_CONTAINER_TOOLS_BUNDLE_DIR` で行うが、通常の local/CI command では `xtask` が内部で設定する。

## CLI

共通形式:

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

- `WORKSPACE` の既定値はカレントディレクトリ。
- `WORKSPACE` は実在するディレクトリでなければならない。
- Git repository 内では repository root を workspace root とする。Git repository でなければ指定ディレクトリを workspace root とする。
- v0.1 では `devcontainer.json` を必須とする。decune TOML は overlay であり、base image/build/Compose 定義の置き換えには使わない。
- CLI output、log、error text は英語にする。
- 設定変更が既存 container/project に反映できない場合、`up` は暗黙 rebuild を行わず、`Run decune rebuild` を促して終了する。

### `up`

```text
decune up [OPTIONS] [WORKSPACE]
```

役割:

- devcontainer を作成または起動し、remote user の shell にログインする。
- image/Dockerfile mode では単一 container を作成または起動する。
- Compose mode では Compose project を作成または起動し、primary service container にログインする。
- 既に起動済みで config hash が一致する場合、作成処理をスキップし、shell ログインのみ行う。
- decune host daemon、credential bridge、port forwarder は `up` process が生きている間だけ動作する。

主要 option:

- `--config <PATH>`: devcontainer metadata file を明示する。relative path は workspace root 相対。
- `--detach`: shell に接続せず起動だけ行う。
- `--rebuild`: 既存 container/project を破棄または再作成する。decune 管理 volume は保持する。
- `--no-cache`: Dockerfile build、Compose service build、Feature layer build で cache を使わない。
- `--pull`: base image または Compose service image を pull してから build/create する。Compose mode では config hash が一致する running container でも reuse fast path に入らず、pulled image を反映するため `docker compose up -d --force-recreate` まで進む。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `-p, --port <SPEC>`: manual forwarding。例: `3000`, `3000:3000`, `127.0.0.1:8080:3000`。複数指定可。Compose mode で service を指定したい場合は devcontainer `forwardPorts` の `"service:port"` を使う。

`--detach` では `up` process 終了時に host daemon も停止するため、manual/automatic forwarding と Git HTTPS host-helper は維持されない。detached container で外部公開が必要な port は、image/Dockerfile mode では `appPort`、Compose mode では Compose file の `ports` を使う。`--detach` と CLI `-p` / `--port` の併用は error とする。設定由来の `forwardPorts` / `[[ports]]` は warning を出して無視する。

### `rebuild`

```text
decune rebuild [OPTIONS] [WORKSPACE]
```

`up --rebuild` と同等の明示サブコマンドである。既存 container/project を停止・削除または force recreate し、再 build/create/start する。decune 管理 volume は保持する。

主要 option:

- `--detach`
- `--no-cache`
- `--pull`
- `--update-features`: feature lock より registry/tag の再解決を優先する。
- `-p, --port <SPEC>`

Compose mode では、`docker compose build` と `docker compose up -d --force-recreate` を使う。`--no-cache` は Compose service build と Feature layer build の両方に適用する。`--pull` は Compose service build/pull に適用するが、decune generated local image を親にする Feature / UID/GID / entrypoint shim layer build には適用しない。

### `down`

```text
decune down [--timeout <SECONDS>] [WORKSPACE]
```

- image/Dockerfile mode: decune 管理 container を停止する。volume、state、image は削除しない。
- Compose mode: decune 管理 Compose project を停止する。volume、state、image は削除しない。`runServices` 指定時も、Compose が `depends_on` 等で起動した dependency service を残さないよう project 全体を停止対象にする。
- Compose mode で現在の `devcontainer.json` / `dockerComposeFile` が削除、移動、または service rename 等で既存 resource と一致しない場合も、state または Docker label から decune 管理 Compose project を特定して停止する。
- 現在の設定が Compose mode でも、同じ workspace に過去の image/Dockerfile mode 由来の decune-managed container が残っている場合は停止する。

明示的な `decune down` は `shutdownAction` に関係なく停止を行う。

### `clean`

```text
decune clean [--force] [--images] [WORKSPACE]
```

- image/Dockerfile mode: managed container、managed volume、state/runtime を削除する。`--images` 指定時だけ generated image を削除する。
- Compose mode: managed Compose project を `docker compose down --volumes --remove-orphans` 相当で削除し、state/runtime を削除する。external volume/network は Compose の標準挙動に従い削除しない。`--images` 指定時だけ decune generated image を削除する。user が Compose file で指定した image を `--rmi all` で削除してはならない。
- Compose mode で現在の `devcontainer.json` / `dockerComposeFile` が削除、移動、または service rename 等で既存 resource と一致しない場合も、state または Docker label から decune 管理 Compose project を特定して削除する。
- 現在の設定が Compose mode でも、同じ workspace に過去の image/Dockerfile mode 由来の decune-managed container や managed volume が残っている場合は削除する。

TTY でない `clean` without `--force` は確認不能として error にする。

## devcontainer.json サポート

### 検出順序

workspace root から以下の順で検出する。

1. `.devcontainer/devcontainer.json`
2. `.devcontainer.json`
3. `.devcontainer/<name>/devcontainer.json`

`--config <PATH>` が指定された場合は自動検出を行わず、その path を devcontainer metadata file として使う。relative path は workspace root 相対で解決する。3 に複数候補がある場合、v0.1 では自動選択せず、`--config .devcontainer/<name>/devcontainer.json` で明示する。

### 構成 mode の判定

| mode | 必須 property | 禁止 property | 備考 |
| --- | --- | --- | --- |
| image | `image` | `build`, `dockerComposeFile`, `service` | image を pull して container を作る |
| Dockerfile | `build.dockerfile` | `image`, `dockerComposeFile`, `service` | Dockerfile を build して container を作る |
| Docker Compose | `dockerComposeFile`, `service` | `image`, `build` | Compose が image/build を持つ |

`dockerComposeFile` と `service` は片方だけ指定してはならない。`runServices` は Compose mode 専用であり、指定する場合は `dockerComposeFile` と `service` も必須である。

### 対応プロパティ

| property | image | Dockerfile | Compose | 備考 |
| --- | --- | --- | --- | --- |
| `image` | yes | no | no | image-based mode |
| `build.dockerfile` | no | yes | no | Dockerfile-based mode |
| `build.context` | no | yes | no | `devcontainer.json` からの相対 path |
| `build.args` | no | yes | no | string value のみ |
| `build.options` | no | partial | no | Docker build argv に渡す。decune 管理 option と context path は不可 |
| `build.target` | no | yes | no | multi-stage build target |
| `build.cacheFrom` | no | partial | no | Docker CLI で扱える形式 |
| `dockerComposeFile` | no | no | yes | string / string array。local path のみ |
| `service` | no | no | yes | primary service |
| `runServices` | no | no | yes | 未指定時は全 service。primary service は常に含める |
| `features` | yes | yes | yes | Compose mode は primary service final image に適用 |
| `overrideFeatureInstallOrder` | yes | yes | yes | Feature install order に反映 |
| `overrideCommand` | yes | yes | yes | image/Dockerfile 既定 true、Compose 既定 false |
| `mounts` | partial | partial | partial | bind/volume 対応。Compose mode は primary service に override として追加。tmpfs は v0.1 error |
| `workspaceMount` | yes | yes | no | Compose mode は Compose file の `volumes` を使う |
| `workspaceFolder` | yes | yes | yes | Compose mode の既定は `/` |
| `containerEnv` | yes | yes | yes | Compose mode は primary service `environment` override。secret storage ではない |
| `remoteEnv` | yes | yes | yes | exec/lifecycle/shell に適用。`${localEnv:...}` 由来 value は argv/log redaction 対象 |
| `remoteUser` | yes | yes | yes | shell/lifecycle user |
| `containerUser` | yes | yes | yes | Compose mode は primary service `user` override |
| `updateRemoteUserUID` | yes | yes | yes | Linux host で既定 true |
| `userEnvProbe` | yes | yes | yes | `none`, `loginShell`, `interactiveShell`, `loginInteractiveShell` |
| `forwardPorts` | yes | yes | yes | Compose mode は `"service:port"` を受け付ける |
| `portsAttributes` | partial | partial | partial | `label`, `onAutoForward`, `requireLocalPort`。`protocol`, `elevateIfNeeded` は warning して無視 |
| `otherPortsAttributes` | partial | partial | partial | automatic forwarding の既定。unsupported fields は warning |
| `appPort` | yes | yes | no | Compose mode は Compose file の `ports` を使う |
| `runArgs` | partial | partial | no | Compose mode は Compose file の service attributes を使う |
| `init` | yes | yes | yes | Compose mode は primary service `init` override |
| `privileged` | yes | yes | yes | Compose mode は primary service `privileged` override |
| `capAdd` | yes | yes | yes | Compose mode は primary service `cap_add` override |
| `securityOpt` | yes | yes | yes | Compose mode は primary service `security_opt` override |
| lifecycle commands | yes | yes | yes | Feature metadata 由来 command は user command より前に実行 |
| `waitFor` | partial | partial | partial | parse するが attached `up` は `postAttachCommand` まで同期実行 |
| `name` | ignored | ignored | ignored | runtime behavior には使わない |
| `shutdownAction` | partial | partial | partial | attached `up` 終了時に適用。明示 `down` / `clean` が正 |
| `hostRequirements` | ignored | ignored | ignored | warning |
| `customizations` | ignored | ignored | ignored | preserve するが実行しない |

### JSONC

`devcontainer.json` は JSON with Comments として扱う。コメント除去を正規表現で実装しない。trailing comma は JSONC として受け付ける。

### `runArgs` allowlist

v0.1 で image/Dockerfile mode が受け付ける `runArgs` は以下のみ。

- `--init`
- `--privileged`
- `--cap-add <CAP>`
- `--security-opt <OPT>`
- `--add-host <HOST:IP>`
- `--dns <IP>`
- `--dns-search <DOMAIN>`

上記以外は unsupported error とする。`--publish` / `-p` は `appPort` または decune forwarding、`--mount` / `--volume` は `mounts`、`--user` は `containerUser`、環境変数は `containerEnv` を使う。

Compose mode では `runArgs` を unsupported error とする。Compose service の `init`、`privileged`、`cap_add`、`security_opt`、`extra_hosts`、`dns`、`dns_search`、`ports`、`volumes`、`user`、`environment` などを Compose file に書くか、Dev Container の cross-orchestrator property を使う。

### `workspaceMount` / `workspaceFolder`

image/Dockerfile mode では、`workspaceMount` を明示する場合は `workspaceFolder` も明示必須とする。`workspaceFolder` は workspace mount target 配下でなければならない。`workspaceMount` 未指定時は `/workspaces/<localWorkspaceFolderBasename>` を bind mount target とし、`workspaceFolder` 未指定時はその target を working directory とする。

Compose mode では `workspaceMount` は unsupported error とする。workspace の mount は Compose file の primary service `volumes` に定義する。`workspaceFolder` 未指定時の既定は `/` である。

## Docker Compose mode

### Compose file 解決

`dockerComposeFile` は string または string array である。各 path は `devcontainer.json` のある directory から相対解決する。絶対 path は portable でないため warning 対象とする。path escape は許可するが、state/hash には canonical path と file digest を含める。存在しない path は error とする。

解決した Compose file は指定順に `docker compose -f <file>` へ渡す。後続 file が前 file を override/add する Compose 標準の merge semantics に従う。relative path resolution の基準は Docker Compose CLI の標準挙動に合わせ、第一 Compose file の parent directory を project directory とする。必要に応じて `--project-directory <first-compose-file-parent>` を明示する。Docker Compose child process の current directory も project directory に固定し、Compose interpolation の `.env` 解決が decune 呼び出し元 PWD ではなく Compose project directory 基準になるようにする。第一 Compose file が symlink の場合、project directory は final symlink を辿った canonical path の parent ではなく、`devcontainer.json` 相対で解決した入力 path の parent とする。

`dockerComposeFile` から git URL、OCI artifact、stdin を参照する構成は v0.1 では unsupported error とする。

### Compose project name

decune は Compose project name を必ず明示する。top-level `name:`、`COMPOSE_PROJECT_NAME`、current directory basename に依存しない。

```text
decune-<safe_workspace_slug>-<workspace_id>
```

- lowercase ASCII、decimal digits、dash のみ。
- 先頭は `decune-` 固定。
- `workspace_id = hex(sha256(canonical_workspace_path))[0..12]`。
- config hash は project name に含めない。同じ workspace の rebuild で project name は安定する。

Compose CLI には `--project-name <project>` を渡す。`COMPOSE_PROJECT_NAME` が host env に存在しても、CLI flag を優先する。

### Compose 正規化と検証

Compose mode の計画作成時、decune は以下を実行する。

```text
docker compose --project-name <project> --project-directory <dir> -f <file>... config --format json
```

この出力を canonical Compose model として扱う。decune は Compose YAML を直接 parse しない。

検証:

- `service` が canonical model の `services` に存在する。
- `runServices` の各 service が canonical model の `services` に存在する。
- primary service の実行 container が一意に決まる。`docker compose ps --format json <service>` が 0 件または 2 件以上を返す状態で shell/lifecycle を実行しない。
- profile により primary service が無効になる構成は error。必要な profile は host env `COMPOSE_PROFILES` または Docker Compose CLI の標準手段で有効化する。
- `workspaceFolder` は absolute path でなければならない。

### generated Compose override

Compose mode で decune 固有機能を適用するため、state/runtime directory に generated override file を作る。この file は user が編集しない。

目的:

- primary service に decune label を付与する。
- primary service image を Feature/UID/GID/entrypoint 適用済み final image に差し替える。
- primary service image を decune generated local image に差し替える場合、元 Compose service の `pull_policy` を引き継いで registry pull しないよう、generated override で `pull_policy: never` を明示する。
- `containerEnv`、`containerUser`、`init`、`privileged`、`capAdd`、`securityOpt`、`mounts`、dotfiles mount、credential/runtime mount を primary service に追加する。
- `overrideCommand = true` の場合、primary service command を keepalive command に差し替える。
- secret value は override file に書かない。GitHub token は host runtime file を bind mount し、token value 自体は file content にのみ存在する。

Generated override file は user の `dockerComposeFile` より後に `-f` で渡す。計画作成時の検証、primary service/container 解決、config hash に含める canonical Compose model は user の `dockerComposeFile` だけを `docker compose config --format json` で正規化した model とする。Generated override 自体は Compose YAML として decune が生成し、hash には final canonical model ではなく generated override semantic hash input として別に含める。

### runServices

- `runServices` 未指定: `docker compose up -d` を service 引数なしで実行し、Compose model 上の有効 service を起動対象にする。
- `runServices` 指定あり: primary `service` と `runServices` の和集合を service 引数として `docker compose up -d <services...>` に渡す。
- image / Dockerfile mode、または `dockerComposeFile` と `service` が揃っていない構成で `runServices` を指定した場合は error とする。
- service dependencies の起動順、`depends_on`、healthcheck、profiles の扱いは Compose CLI に委譲する。
- `down` / attached `up` 終了時の `stopCompose` は、`runServices` の service 引数で対象を狭めず、Compose project 全体を停止する。これは Compose が `depends_on` 等で暗黙に起動した dependency service を残さないためである。`clean` は project 全体を削除対象にする。

### Build / pull / recreate

Compose mode の image creation は次の順で行う。

1. `initializeCommand` を host で実行する。
2. user Compose file だけで `docker compose config --format json` を実行し、primary service の base image/build 情報を検証する。
3. `docker compose build` または `docker compose up -d --build` で primary service と必要な service image を準備する。`--no-cache` と `--pull` は Compose build option に反映する。
4. primary service の base image を特定する。Compose service に `build` がある場合は Compose が tag した service image を使う。`image` がない build-only service では Compose の既定 tag `<project-name>-<service>` を使う。service に `image` のみがある場合はその image を使い、metadata 解決前に missing image を pull する。
5. Feature、UID/GID sync、entrypoint shim が必要な場合、base image に decune generated layer を重ね、decune generated image tag を作る。
6. generated Compose override に primary service image 差し替えを反映する。decune generated local image に差し替える場合は `pull_policy: never` も反映する。
7. generated override 込みで `docker compose up -d` を実行する。`--pull` または `rebuild` の場合は `--force-recreate` を渡す。
8. `docker compose ps --format json` と `docker inspect` で primary container ID を解決し、lifecycle と shell attach に進む。

`--pull` は user Dockerfile build、base image pull、Compose service build/pull にだけ適用する。Feature、UID/GID sync、entrypoint shim などの decune generated layer は直前に準備した local image tag を `FROM` にすることがあるため、これらの layer build には Docker build の `--pull` を渡さない。

Dockerfile-based mode の `build.options` は、Docker build の context 引数 `-` より前に argv として渡す。shell 文字列には連結しない。decune が管理する `-f` / `--file`、`-t` / `--tag`、`--label`、`--build-arg`、`--target`、`--cache-from`、`--no-cache`、`--pull`、`--rm` / `--force-rm`、`--iidfile`、`--metadata-file`、`--output` などの option は `build.options` では指定できない。`build.options` は option だけを受け付け、build context path は decune が stdin tar と最後の `-` で管理する。

`--platform`、`--ssh`、`--secret`、`--add-host`、`--network` など Docker CLI に委譲できる build option は指定できる。ただし `build.options` の値は argv に出るため、secret 文字列そのものを直接書かない。secret は `--secret id=npm,env=NPM_TOKEN` のように host 環境変数や file path を参照する形にする。

`rebuild` は generated image と Compose service を再作成する。anonymous volume は保持する。`clean --images` 以外で user image や Compose service image を削除してはならない。

### shutdownAction

Dev Container の既定値に合わせる。

- image/Dockerfile mode 既定: `stopContainer`
- Compose mode 既定: `stopCompose`

attached `up` で shell が終了したとき:

- `none`: container/project を停止しない。
- `stopContainer`: primary container だけ停止する。
- `stopCompose`: Compose mode では Compose project 全体を停止する。image/Dockerfile mode では `stopContainer` と同じ。

明示的な `decune down` / `decune clean` は user 操作として扱い、`shutdownAction` によって no-op にはしない。

## decune TOML 設定

### 配置

- global: `$XDG_CONFIG_HOME/decune/config.toml`
- global fallback: `~/.config/decune/config.toml`
- project: `<workspace>/.decune/config.toml`

project 設定は Git 管理してよい。秘密情報を設定 file に直接書かない。

### merge 順序

最終設定は以下の順で合成する。後勝ちが基本である。

1. decune default
2. image metadata の `devcontainer.metadata`
3. Feature metadata
4. global decune config
5. `devcontainer.json`
6. project decune config
7. CLI flags

`--config <PATH>` は devcontainer metadata file を選択するだけであり、decune TOML overlay の追加指定ではない。

### merge rule

- scalar: 後勝ち。
- map: key ごとに merge。同一 key は後勝ち。
- decune TOML の array: 原則 append。ただし identity を持つ要素は置換。
- feature identity: canonical Feature ID と concrete ref。同一 concrete ref は option を merge する。`enabled = false` は canonical Feature ID 単位で無効化する。
- mount identity: `target`。
- dotfile identity: `target`。
- port identity: `protocol + service + container + host_ip`。service 未指定は primary service を表す。
- hook identity: identity なし。順序を保って append。

### 設定例

```toml
version = 1
shell = "/bin/zsh"

[features."ghcr.io/devcontainers/features/github-cli:1"]
version = "latest"

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
read_only = true
resolve_symlink = true
on_conflict = "replace-symlink"

[[mounts]]
source = "~/work"
target = "/workspaces/work"
type = "bind"
read_only = false
resolve_symlink = true
create = false

[[ports]]
container = 3000
host = 3000
host_ip = "127.0.0.1"
protocol = "tcp"
require_local = false
label = "web"

[ports.auto]
enabled = true
min = 1024
max = 32768
ignore = [22, 2375, 2376]
on_auto_forward = "notify"

[credentials.git]
enabled = true
copy_user = true
copy_global_config = false
https = "host-helper"
ssh_agent = "auto"

[credentials.github]
enabled = true
mode = "gh-token-file"
install_feature_if_missing = true

[[hooks.before_post_create]]
command = "scripts/before-post-create.sh"
where = "container"
user = "remote"
shell = true
```

### top-level

- `version`: 必須。v0.1 では `1` のみ。
- `shell`: 任意。`decune up` で attach する shell path または command 名。
- 未知の key は error。

### `[features]`

TOML の table key に Feature ref を quote して指定する。

```toml
[features."ghcr.io/devcontainers/features/go:1"]
version = "1.23"
enabled = true
```

- `enabled = false` で global/image metadata/Feature metadata 由来 Feature を project 側から無効化できる。
- `enabled` は decune の予約 key であり、Feature option としては渡さない。
- それ以外の key は Feature option として扱う。

### `[[dotfiles]]`

dotfiles は host path を remote home に直接 bind mount しない。`/opt/decune/dotfiles/<target>` に mount し、container setup 時に remote user の home へ symlink を作る。

- `source`: host path。global config では `~` または absolute path。project config の relative path は workspace root 相対。
- `target`: remote home からの相対 path。absolute path は禁止。
- `enabled`: 既定 true。false の場合は同一 target を無効化。
- `read_only`: 既定 true。
- `resolve_symlink`: 既定 true。true の場合は source を canonicalize する。file の場合は canonicalized source を直接 bind mount する。directory の場合は、配下 symlink がなければ canonicalized source を直接 bind mount する。配下 symlink があり、同一 backing root に完全一致する場合は backing root を直接 bind mount する。完全一致しない場合は state dir に mount 用 skeleton を作成し、skeleton と symlink 解決後の実ファイル/実ディレクトリを追加 bind mount する。skeleton と追加 bind mount の writable/read-only は `read_only` に従う。`read_only = false` の skeleton-only path に container から新規作成された file/directory は、元 source ではなく state dir の skeleton に保存される。dotfile 内容は state dir にコピーしない。broken symlink、循環 symlink、特殊ファイル、mount 数過多など直接 bind mount として表現できない場合は error。
- `on_conflict`: `fail`, `replace-symlink`, `backup`。既定 `fail`。

Compose mode では primary service に dotfiles bind mount と setup lifecycle を適用する。

### `[[mounts]]`

任意の追加 mount。

- `type`: `bind`, `volume`, `tmpfs`。v0.1 では `bind` と `volume` に対応し、`tmpfs` は error。
- `source`: `bind` では必須。`volume` では volume 名。
- `target`: container absolute path。`/opt/decune` と `/run/decune` 配下、および workspace mount target と同一 target は禁止。
- `enabled`: 既定 true。false の場合は同一 target を無効化。
- `read_only`: 既定 false。
- `resolve_symlink`: bind source にのみ適用。既定 true。
- `create`: `false`, `"directory"`。既定 false。file の自動作成は行わない。

Compose mode では primary service に generated override として追加する。

### `[[ports]]`

manual forwarding 設定。Docker publish ではない。

- `container`: container 側 port。必須。
- `host`: host 側 port。省略時は `container` と同じ番号を試し、占有済みなら空き port を探索する。
- `host_ip`: 既定 `127.0.0.1`。`0.0.0.0` は明示された場合のみ許可。
- `protocol`: v0.1 は `tcp` のみ。
- `service`: Compose mode で対象 service を指定する任意 field。未指定は primary service。image/Dockerfile mode では指定不可。
- `enabled`: 既定 true。
- `require_local`: true の場合、host port が占有済みなら別 port に fallback せず失敗。
- `label`: 表示用。

### `[ports.auto]`

- `enabled`: 既定 true。
- `min`: 既定 1024。
- `max`: 既定 32768。
- `ignore`: automatic forwarding から除外する port。
- `on_auto_forward`: `notify`, `silent`, `ignore`。browser/preview 系は CLI では `notify` 相当。

Compose mode の automatic forwarding は primary service の container を対象にする。sidecar service は明示 `forwardPorts` または `[[ports]].service` で指定する。

### `[credentials.git]`

```toml
[credentials.git]
enabled = true
copy_user = true
copy_global_config = false
https = "host-helper"
ssh_agent = "auto"
```

- `enabled`: 既定 true。
- `copy_user`: host の `git config --global user.name` / `user.email` を container の remote user に設定する。既定 true。
- `copy_global_config`: `~/.gitconfig` 全体を container にコピーする。既定 false。
- `https`: `off`, `host-helper`。既定 `host-helper`。
- `ssh_agent`: `off`, `auto`, `required`。既定 `auto`。

`host-helper` は container 内に `git-credential-decune` を配置し、host daemon 経由で host の `git credential fill/approve/reject` を呼ぶ。helper は container OS/arch 用 artifact であり、host の `decune` binary をそのまま bind mount しない。

### `[credentials.github]`

```toml
[credentials.github]
enabled = true
mode = "gh-token-file"
install_feature_if_missing = true
```

- `enabled`: 既定 true。
- `mode`: `off`, `gh-token-file`。既定 `gh-token-file`。
- `install_feature_if_missing`: host token が取得でき、container に `gh` がない場合に `ghcr.io/devcontainers/features/github-cli:1` を追加する。既定 true。

`gh-token-file` は host の `gh auth token` を実行し、runtime directory に mode 0600 の token file を作る。container には `/run/decune/secrets/github-token` として read-only file mount する。`GH_CONFIG_DIR=/run/decune/gh` は writable ephemeral directory として分離する。

Token value は argv、image layer、Docker/Compose label、container env、state、config hash、generated Compose override file に入れない。ただし container 内プロセスは token file に到達できるため、untrusted repository では `[credentials.github].enabled = false` を推奨する。

### `[[hooks.*]]`

利用可能な hook 名:

- `before_initialize`
- `after_initialize`
- `before_on_create`
- `after_on_create`
- `before_update_content`
- `after_update_content`
- `before_post_create`
- `after_post_create`
- `before_post_start`
- `after_post_start`
- `before_post_attach`
- `after_post_attach`

hook entry:

```toml
[[hooks.before_post_create]]
command = "scripts/setup.sh"
where = "container"
user = "remote"
shell = true
```

- `command`: string または string array。array は 1 要素以上。
- `where`: `host`, `container`。`initialize` 系は host のみ。
- `user`: `remote`, `root`, `<name>`。container hook のみ。既定 `remote`。
- `shell`: true なら `/bin/sh -lc` で実行。string command の既定は true、array command の既定は false。
- `workdir`: 省略時、host hook は workspace root、container hook は `workspaceFolder`。

## 変数展開と path

以下を string value で展開する。

- `${localEnv:VAR}` / `${localEnv:VAR:default}`
- `${containerEnv:VAR}` / `${containerEnv:VAR:default}`
- `${localWorkspaceFolder}` / `${localWorkspaceFolderBasename}`
- `${containerWorkspaceFolder}` / `${containerWorkspaceFolderBasename}`
- `${devcontainerId}`
- `${uid}` / `${gid}`
- `${remoteUser}`
- `${remoteUserHome}`

`${remoteUserHome}` は `/home/<user>` と推測せず、container/image 内の passwd database から解決する。`containerEnv` 自体の中で `${containerEnv:...}` を使う構成は v0.1 では error とする。

`${localEnv:...}` から展開された `containerEnv` / `remoteEnv` value は secret-sensitive として追跡する。decune はその実値を state、config hash、generated Compose override、Docker/Compose label、argv、通常の error log に平文保存してはならない。config hash では key を保持し、`containerEnv` は container 再作成判定のため実値ではなく非可逆 digest を含め、`remoteEnv` は redacted marker に置き換える。Compose mode の generated override では primary service `environment` に `${DECUNE_CONTAINER_ENV_<SAFE_KEY>}` 形式の placeholder を書き、実値は `docker compose` child process の environment として渡す。placeholder variable name の `<SAFE_KEY>` は `containerEnv` key から ASCII alphanumeric / underscore のみへ正規化した値とする。

`containerEnv` は container 作成時の環境変数であり、container 内プロセスや Docker inspect から見える。decune は `containerEnv` を secret storage として保証しない。literal に書かれた secret 文字列や、decune が `${localEnv:...}` 由来と追跡できない値は自動では secret-sensitive と判定しない。

host bind source の処理順:

1. `~` を展開。
2. `${...}` を展開。
3. relative path を基準 directory から absolute path にする。
4. `create = "directory"` なら directory を作成。
5. `resolve_symlink = true` なら canonicalize。
6. 存在しない path は `create` が指定されていない限り error。

Compose file 内の environment interpolation は Docker Compose CLI に委譲する。decune は `devcontainer.json` と decune TOML の値だけを自前で展開する。

## Runtime adapter

### 原則

`decune` 本体は外部コマンドを shell 文字列で実行しない。`std::process::Command` / `tokio::process::Command` に argv 配列を渡す。log には必要最小限の command name と sanitized argv を出す。secret value を argv に入れる必要がある設計は禁止する。

adapter:

- `DockerCli`: `docker` の存在確認、version、image/container/exec/cp/inspect/build/pull/rm/stop/start/wait/port 相当。
- `DockerComposeCli`: `docker compose` の存在確認、`version --short`、required capability probe、config/build/up/stop/down/ps/logs/pull 相当。
- `RuntimeCommand`: command 実行、stdout/stderr capture、streaming、exit status、timeout、signal handling、redaction の共通基盤。

JSON を読む操作は、CLI の JSON 出力を serde 型へ parse する。

- `docker image inspect --format json` または `docker inspect --format json`
- `docker compose config --format json`
- `docker compose ps --format json`

Docker CLI / Compose CLI の実行失敗は、実行した高レベル action、対象 resource、exit status、stderr の短い抜粋を含む error に変換する。stderr 全文に secret が混じる可能性がある場合は redaction rule を通す。

### compatibility

- Docker CLI は Docker daemon と同じ host/remote context を指す。
- Compose CLI は Docker CLI と同じ `DOCKER_HOST` / `DOCKER_CONTEXT` / `DOCKER_CONFIG` を継承する。
- Podman 互換 endpoint は、Docker CLI / Compose CLI が透過的に扱える範囲でのみ対象にする。Podman Compose 固有挙動は v0.1 の公式対象外。

## Docker resource と state

workspace id:

```text
hex(sha256(canonical_workspace_path))[0..12]
```

image/Dockerfile mode の Docker resource name には workspace basename をそのまま使わず、ASCII safe slug と workspace id を組み合わせる。

- container: `decune-<safe_workspace_slug>-<workspace_id>`
- image: `decune/<safe_workspace_slug>-<workspace_id>:<config_hash>`
- state directory: `$XDG_STATE_HOME/decune/<workspace_id>` または `~/.local/state/decune/<workspace_id>`
- runtime directory: `$XDG_RUNTIME_DIR/decune/<workspace_id>` または `/tmp/decune-<uid>/<workspace_id>`

Compose mode:

- project: `decune-<safe_workspace_slug>-<workspace_id>`
- generated primary image: `decune/<safe_workspace_slug>-<workspace_id>:<config_hash>`
- generated Compose override: `$XDG_STATE_HOME/decune/<workspace_id>/compose.override.yaml`
- state/runtime directory は image/Dockerfile mode と同じ。

主な decune label:

- `decune.managed=true`
- `decune.workspace=<canonical_workspace_path>`
- `decune.workspace_id=<workspace_id>`
- `decune.config_hash=<hash>`
- `decune.version=<version>`
- `devcontainer.local_folder=<canonical_workspace_path>`
- `devcontainer.config_file=<path>`

Compose mode では上記 label を primary service に追加する。明示的な sidecar service forwarding 対象 service には、forwarding runtime mount の再作成判定に必要な `decune.managed=true` と `decune.workspace_id=<workspace_id>` を追加する。Compose が付与する `com.docker.compose.project` と `com.docker.compose.service` も container identity に使う。`com.docker.compose.*` prefix を decune の generated override で上書きしてはならない。

既存 container/project の再利用は `decune.managed=true` と `decune.workspace_id` が一致するものに限る。他ツールの container は拾わない。

config hash には、resolved metadata/config、Feature lock、relevant CLI flags、Dockerfile 内容、`build.options`、effective ignore file、build context digest、entrypoint plan、Linux host の UID/GID sync input、Compose mode の user Compose files から得た sanitized canonical Compose model、Compose file digest、generated override semantic hash input を含める。manual/automatic forwarding の現在値、credential token value、SSH agent socket path、GitHub token file path、`${localEnv:...}` 由来の `remoteEnv` value、Compose secrets の解決済み value は含めない。`${localEnv:...}` 由来の `containerEnv` value は平文では含めず、container 作成時環境の変更を検出するため非可逆 digest として含める。Compose mode では user Compose files だけを対象にした `docker compose config --format json` が解決した interpolation / env file / profile / merge 結果から、`services.<service>.environment` の leaf value を平文ではなく digest marker に置き換えた canonical Compose model を hash に含める。この digest input は `decune-compose-env-value-hash-v1` で domain-separated / versioned にし、JSON path、JSON value type、canonical JSON value を含める。digest marker は `decune-compose-env-value-hash-v1:sha256:<hex>` 形式とし、environment value の平文を state、label、log、config hash input に残してはならない。generated override semantic hash input には primary service、decune が追加する label / environment / mount / user / security option / startup command、および decune generated image へ差し替えるかどうかを含める。`${localEnv:...}` 由来の `containerEnv` value は redacted marker または placeholder として扱い、実値を content hash 入力にしない。ただし generated override 内の `decune.config_hash` label や hash 由来 image tag など、hash 自身から派生する値は循環を避けるため hash 入力にしない。

state file は `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` に保存する。write は atomic に行う。Docker/Compose label と state が矛盾する場合、container/project identity と config hash は runtime label を正とする。lifecycle 完了 flag と devcontainer config file path は state に記録し、creation lifecycle の二重実行や `up --config` 後の Compose project lifecycle 復元に使う。

## Build と Features

### image-based

1. base image を pull する。`--pull` 指定時は常に pull を試す。
2. Feature があれば Feature 適用済み image を build する。
3. Linux host で UID/GID sync が必要なら sync layer を build する。
4. collected entrypoint があれば generated entrypoint shim layer を build する。
5. Feature、UID/GID sync、entrypoint shim が不要なら base image をそのまま create に使う。

### Dockerfile-based

1. `build.context` と `build.dockerfile` を `devcontainer.json` 相対で解決する。
2. Dockerfile-specific ignore file `<Dockerfile>.dockerignore` があれば context root の `.dockerignore` より優先する。
3. Docker CLI build へ tar context または context directory を渡す。
4. Dockerfile build 結果 image に Feature を重ねる。
5. 必要なら UID/GID sync layer と entrypoint shim layer を重ねる。

v0.1 では Dockerfile が build context 外にある構成を unsupported error とする。Dockerfile-based final image の `devcontainer.metadata` label は config hash と final image tag 決定の循環を避けるため merge せず、検出時は warning に留める。

### Docker Compose-based

Compose primary service の image/build を base image として扱う。Feature は primary service の final image にだけ適用する。sidecar service には Feature、UID/GID sync、entrypoint shim、dotfiles、credentials を自動適用しない。

primary service に `build` がある場合、まず Compose CLI で service image を build する。primary service に `image` のみがある場合、必要に応じて pull する。base image 解決後、image/Dockerfile mode と同じ Feature/UID/GID/entrypoint layer pipeline を適用し、generated Compose override で primary service image を final image に差し替える。

Feature:

- OCI registry ref と local `./` ref に対応する。
- direct HTTPS tgz Feature は v0.1 では未対応。
- registry auth は Docker CLI 互換で `credHelpers`、`credsStore`、`auths` の順に source を選ぶ。選択 source が失敗しても別 source に fallback しない。
- manifest body と layer blob は sha256 digest を検証する。
- local Feature path は `devcontainer.json` directory からの相対 `./` path に限定し、absolute path と path escape を拒否する。
- local Feature directory basename と `devcontainer-feature.json` の `id` は一致必須。
- `devcontainer-feature.json` と `install.sh` は必須。
- OCI Feature は `<workspace>/.decune/features.lock.toml` に digest lock を記録する。
- `rebuild --update-features` は lock より再解決を優先する。
- Feature metadata は required field `id`, `version`, `name` を要求する。
- Feature option は Features 仕様に従って env key に変換し、default option も export する。env key collision は error。

## Container create/start と user

image/Dockerfile mode では、workspace mount 未指定時は `/workspaces/<localWorkspaceFolderBasename>` へ bind mount する。

Compose mode では workspace mount を自動追加しない。primary service の Compose `volumes` に workspace bind mount がない場合でも decune は起動を続けるが、`workspaceFolder` が存在しない場合は lifecycle/shell 実行前に error とする。

user 解決:

- effective container user: `containerUser`、image/Feature metadata `containerUser`、Compose service `user`、Docker image config `User`、`root`。
- effective remote user: `remoteUser`、image/Feature metadata `remoteUser`、effective container user。

存在しない effective remote user は root fallback せず configuration error とする。numeric UID/GID は passwd entry がなくても runtime identity として扱えるが、home directory が必要な処理では error または warning skip になる。

`updateRemoteUserUID` は Linux host で既定 true。remote user が明示されていれば remote user、なければ `containerUser`、image/Feature metadata `containerUser`、Compose service `user` のいずれかで container user が明示されている場合に container user を sync target とする。非 Linux host、root target、`updateRemoteUserUID = false`、passwd entry がない numeric target は no-op または warning skip とする。

Compose mode で UID/GID sync が必要な場合、primary service base image に sync layer を重ねた final image を作る。running container 内で `/etc/passwd` を直接 mutation しない。
UID/GID sync によって runtime user 表現が変わる場合、generated Compose override の primary service `user` には sync 後の user/group を反映し、元の numeric UID/GID で primary process を起動しない。

## Lifecycle と shell attach

Dev Container lifecycle は以下の順で扱う。

1. `initializeCommand`（host）
2. `onCreateCommand`
3. `updateContentCommand`
4. `postCreateCommand`
5. `postStartCommand`
6. `postAttachCommand`

`initializeCommand` は image creation / Compose project creation より前に実行する。container lifecycle command は primary container 内で実行する。

decune hook は各 lifecycle stage の前後に実行する。Feature metadata 由来 lifecycle command は Feature install order 順に収集し、user の `devcontainer.json` 由来 command より先に実行する。

lifecycle command が失敗した場合、対応する after hook と後続処理は実行しない。creation lifecycle の成功済み stage は state に記録し、次回 reuse 時に二重実行しない。

non-detach `up` / `rebuild` は lifecycle 後に remote user shell を TTY attach し、shell exit code を CLI exit code として返す。shell attach は `docker exec` 相当の CLI adapter で primary container に対して実行する。Compose mode でも `docker compose exec` ではなく、container ID を解決して `docker exec` 相当を使ってよい。

`--detach` では attach lifecycle、forwarding listener、`postAttachCommand`、shell attach を実行しない。

## Git/GitHub 認証

### Git HTTPS

`[credentials.git].https = "host-helper"` の場合、container 内に `git-credential-decune` を配置し、Git credential helper として設定する。helper は host daemon に versioned JSON request を送り、host の `git credential fill/approve/reject` を実行する。

### SSH agent

`ssh_agent = "auto"` では host の `SSH_AUTH_SOCK` が Unix socket の場合のみ forwarding を設定する。container env の `SSH_AUTH_SOCK` は `/run/decune/ssh-agent.sock`。`ssh_agent = "required"` で socket が利用できない場合は error。

Compose mode では SSH agent socket mount は primary service にのみ追加する。

### GitHub CLI

host の `gh auth token` が成功した場合、token を runtime directory に mode 0600 の file として作り、container には `/run/decune/secrets/github-token` として read-only mount する。`GH_CONFIG_DIR=/run/decune/gh` は writable ephemeral directory とする。token file は `up` 終了時に scrub し、`down` / `clean` で削除する。

Compose mode では GitHub token file mount は primary service にのみ追加する。

## Port forwarding

`forwardPorts`、decune `[[ports]]`、CLI `-p` は forwarding であり Docker publish ではない。host 側 listen address の既定は `127.0.0.1`。container 内で `127.0.0.1:<container port>` にだけ listen している process にも届くよう、container-side `decune-forward-agent` 経由で proxy する。

`appPort` は image/Dockerfile mode の Docker publish であり container create 時に決まる。host IP が指定されない場合、Docker の既定で全 interface に公開される可能性があるため warning 対象とする。

Compose mode では Docker publish は Compose file の `ports` に委譲する。`appPort` は unsupported error とする。

manual forwarding source priority:

1. CLI `-p`
2. project decune `[[ports]]`
3. devcontainer `forwardPorts`
4. global decune `[[ports]]`

host port が占有済みの場合、`require_local = true` なら失敗し、false なら昇順で空き port を探索する。

Compose mode の service 解決:

- `forwardPorts` number: primary service の port。
- `forwardPorts` string `"3000"`: primary service の port。
- `forwardPorts` string `"db:5432"`: Compose service `db` の port。
- `portsAttributes` key `"db:5432"`: Compose service `db` の port attributes。
- `[[ports]].service = "db"`: Compose service `db` の port。

`forwardPorts` の `"service:port"` 形式と `[[ports]].service` は Compose mode 専用である。image/Dockerfile mode では service 名で対象 container を解決できないため unsupported error とする。

sidecar service forwarding は、その service の container ID を解決し、必要な container-side tool を runtime install して forward-agent を起動する。対象 service には forwarding runtime mount と decune identity label だけを generated override で追加し、credentials、dotfiles、GitHub token、SSH agent は自動注入しない。service の replica が 2 以上なら error とする。

automatic forwarding は container agent が `/proc/net/tcp` と `/proc/net/tcp6` を読み、LISTEN port を検出する。既定 scan interval は 2 秒、initial delay は 3 秒。manual forwarding 済み、Docker publish 済み、ignore list、`portsAttributes.onAutoForward = "ignore"` は除外する。Compose mode の automatic forwarding は primary service のみを対象にする。

## Host daemon と security boundary

host daemon は `decune up` の子タスクとして起動し、`up` 終了時に停止する。常駐 system daemon ではない。

責務:

- Git credential helper request の処理。
- GitHub token file の一時管理。
- port forwarding runtime の socket 基盤。

禁止:

- container から任意 host command を実行する API を提供しない。
- Docker socket を container に暗黙 mount しない。
- Compose project に user が指定していない Docker socket mount を追加しない。

runtime directory は 0700、socket は 0600 を基本とする。permission 調整時も host daemon は Unix socket peer UID を検証する。

Security note:

- `decune up` は Dockerfile、Compose service build、local/OCI Feature の `install.sh`、Feature/lifecycle command、hook、`userEnvProbe` 対象 shell startup file を実行し得る。
- devcontainer metadata と Compose file は bind mount、`privileged`、`capAdd`、`securityOpt`、port publish、SSH agent forwarding、Git/GitHub credential forwarding により host や secret への強い到達性を container へ与え得る。
- GitHub token forwarding を有効にすると、container 内 process は token file にアクセスできる。
- untrusted repository では `.devcontainer/`、Compose file、local Feature を確認し、必要に応じて `[credentials.git].enabled = false` と `[credentials.github].enabled = false` を設定する。

## 検証方針

通常の formatting / lint:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets -- -D warnings
```

Docker / Compose integration test を含む full test:

```sh
docker version
cargo run --locked -p xtask -- workspace-test
cargo run --locked -p xtask -- compose-integration
```

Compose integration test だけを明示実行する場合:

```sh
docker version
docker compose version
cargo run --locked -p xtask -- compose-integration
```

`compose_integration` filter の Docker-backed test は `#[ignore]` として定義する。通常の unit test では実行せず、`cargo run --locked -p xtask -- compose-integration` が Docker daemon と Docker Compose v2 plugin を確認したうえで `cargo test --workspace --all-features --no-fail-fast compose_integration -- --ignored --test-threads=1` を実行する。純粋ロジックだけ確認する場合は、対象 package/module/test 名で filter して実行する。

主な integration test 対象:

- image-based up/down/clean/rebuild。
- Dockerfile build と `--no-cache`。
- Dockerfile-specific ignore file の context hash / tar context 反映。
- Compose string `dockerComposeFile` / array `dockerComposeFile`。
- Compose `service` / `runServices` / profile / multiple file merge。
- Compose primary service Feature install、UID/GID sync、entrypoint shim。
- Compose generated override の label、environment、mount、user、security option 反映。
- Compose sidecar explicit forwarding `"service:port"`。
- Compose project `down` / `clean` が他 project や user image を壊さないこと。
- read-only bind mount と symlink source mount。
- dotfiles symlink setup。
- lifecycle failure と lifecycle 二重実行防止。
- `overrideCommand`、Feature entrypoint shim。
- manual / automatic forwarding。
- `appPort` warning/error と unsupported port attributes warning。
- UID/GID sync。
- Feature metadata required fields、Feature option env/default、local Feature constraints。
- Docker/Compose resource name sanitization。
- non-TTY `clean` without `--force` failure。
- state repair と secret leak regression。
