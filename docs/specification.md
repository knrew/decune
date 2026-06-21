# decune 仕様

この文書は、`decune` の公開挙動、CLI の契約、設定スキーマ、Docker/Compose 連携、状態とリソース、セキュリティ境界の正本である。利用手順は [usage.md](usage.md)、開発・検証手順は [development.md](development.md)、用語基準は [glossary.md](glossary.md) を参照する。

実装作業ログ、milestone 履歴、PR 単位の一時 issue、agent prompt はこの文書に置かない。

## 目的

`decune` は、Dev Containers Specification の Dev Container を Rust 製の単一 CLI から起動、接続、停止、削除するためのツールである。VS Code や Node.js ベースの Dev Container CLI には依存しない。

`decune` は Dev Container の次の 3 構成を正式対象にする。

1. image-based: `image`
2. Dockerfile-based: `build.dockerfile`
3. Docker Compose-based: `dockerComposeFile` + `service`

global/project の decune TOML 設定を Dev Container configuration に重ねる。VS Code Dev Containers が暗黙に提供する Git/GitHub 認証、dotfiles、port forwarding、UID/GID sync も decune の責務として明示的に扱う。

## 文書の責務

- [README.md](../README.md): プロジェクト概要、最短インストール手順、クイックスタート、主要リンク。
- [usage.md](usage.md): 利用者向けの操作手順、操作例、設定例、安全な使い方。
- [specification.md](specification.md): 公開挙動とセキュリティ境界。
- [development.md](development.md): コントリビューター向けの環境構築、検証、リリース成果物の作成コマンド。
- [glossary.md](glossary.md): プロジェクト用語と表記基準。

README と usage はこの仕様を要約できるが、この仕様と矛盾する内容を持ってはならない。仕様、README、実装、test が矛盾する場合は、暗黙に実装を正とせず差分の意図を確認してから揃える。

## 対象範囲

### 対応する挙動

- Rust 製単一バイナリの CLI。
- Docker image / container / exec / copy / inspect 操作を `docker` CLI adapter 経由で行う。
- Docker Compose 操作を `docker compose` v2 CLI adapter 経由で行う。
- `bollard` crate への依存を廃止する。Docker Engine API を Rust 型で直接操作する実装は前提にしない。
- Dev Container の image-based / Dockerfile-based / Docker Compose-based 構成。
- JSONC としての `devcontainer.json` 読み込み。
- TOML による global/project 設定。
- Dev Container Features の OCI registry 取得、digest lock、local Feature、インストール、metadata merge。
- Docker Compose モードでは、Feature、dotfiles、credentials、lifecycle、remote shell、port forwarding を primary service に適用する。
- Git HTTPS credential helper、SSH agent、GitHub CLI token forwarding。
- manual port forwarding と automatic port forwarding。
- Linux host での `updateRemoteUserUID` による UID/GID sync。
- lifecycle command と decune 固有 hooks。
- `up`、`rebuild`、`down`、`remove` / `rm`、`ports` コマンド。
- GitHub Releases のビルド済みアーカイブによる公式配布。

### Docker Compose サポートの定義

この文書における「Docker Compose 完全サポート」とは、Dev Containers Specification が定義する Docker Compose-based 構成を、image/Dockerfile 構成と同じ decune 機能群で扱えることを指す。

具体的には以下を満たす。

- `dockerComposeFile` は string と string array の両方を受け付け、配列順を保持して Compose に渡す。
- `service` を primary service として扱い、remote shell、lifecycle、Feature、dotfiles、credentials、UID/GID sync、automatic forwarding の既定対象にする。
- `runServices` を受け付ける。未指定時は Compose プロジェクトの全 service を起動対象にする。指定時も primary service は必ず起動対象に含める。
- Compose YAML の merge、include、profiles、anchors、extension fields、environment interpolation、relative path resolution、build semantics、network/volume/config/secret semantics は decune が再実装せず、Docker Compose v2 CLI に委譲する。
- decune は `docker compose config --format json` で正規化済み Compose model を取得し、検証、hash、対象 service/container 解決に使う。
- `forwardPorts` の `"service:port"` 形式を Compose service 名として扱い、primary service 以外の明示 forwarding に対応する。
- Compose が作成した resource の lifecycle は Compose プロジェクト単位で管理する。decune は Compose project name を明示指定し、他 project を拾わない。

「完全サポート」は、Compose Specification の全属性を decune が自前で解釈することを意味しない。Compose 仕様の追随は Docker Compose CLI に委譲し、decune は Dev Container と decune 固有機能を Compose project に安全に重ねる責務を持つ。

### 対象外

- 旧 `docker-compose` v1 standalone binary の公式対応。`docker compose` v2 プラグインを必須にする。
- Kubernetes、Swarm stack、Docker Desktop UI、cloud provider 固有 orchestrator の直接サポート。
- Compose file を `dockerComposeFile` から git URL / OCI artifact / stdin で参照する構成。Dev Container の `dockerComposeFile` は `devcontainer.json` からの local path として扱う。
- primary service の replica/scale が 2 以上の構成。remote shell と lifecycle の対象 container が一意に決まらないため error にする。
- VS Code 拡張機能のインストールや `customizations.vscode` の適用。
- GPG agent forwarding。
- コンテナから任意の host command を実行する API。
- Windows host 向け公式配布。
- crates.io または `cargo install --git` による公式インストール。

## ホスト要件

- Linux または macOS host。
- Docker CLI `docker`。
- Docker Compose v2 プラグイン。`docker compose version` が成功し、以下の機能を使えること。
  - `docker compose config --format json`
  - `docker compose ps --format json`
  - `docker compose build --with-dependencies`
  - `docker compose pull --policy always`
  - `docker compose pull --ignore-buildable`
  - `docker compose pull --include-deps`
  - `docker compose up --force-recreate`
  - `docker compose up --remove-orphans`
- Docker デーモンへ接続できる権限。
- Git 認証連携を使う場合: host 側の `git`、必要に応じて `SSH_AUTH_SOCK`。
- GitHub CLI 連携を使う場合: host 側の `gh` と `gh auth token` が成功する状態。

Docker endpoint、context、credential helper、BuildKit、Compose profiles などは Docker CLI / Docker Compose CLI の標準挙動を継承する。decune は `DOCKER_HOST`、`DOCKER_CONTEXT`、`DOCKER_CONFIG`、`COMPOSE_PROFILES` などの host 環境変数を原則としてそのまま子 process に渡す。ただし secret value を log、state、hash、label、image layer に保存してはならない。

## 配布方針

公式配布は GitHub Releases のビルド済みアーカイブを第一導線とし、ソースコードからのローカル `cargo install --path .` を第二導線とする。crates.io publish と `cargo install --git` は公式導線にしない。

リリースアーカイブは以下を含む。

- `decune` binary
- `LICENSE`
- `README.md`

release asset:

- `decune-v{version}-{host_triple}.tar.gz`
- `SHA256SUMS`
- `release-manifest.json`

`scripts/install.sh` はリリースアーカイブのインストール補助として提供する。latest 自動解決は行わず、利用者が指定した version の OS/arch 対応 asset を取得し、`SHA256SUMS` で検証してからインストールする。

初期 target:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

container-side tools は release build 時に host binary へ埋め込む。Git repository には生成済み binary artifact を入れない。

container-side tool platform:

- `linux-amd64`
- `linux-arm64`

release asset は `SHA256SUMS` で検証できる。GitHub Actions release workflow は build provenance attestation を作成し、release publish 前に全 asset を draft release に添付する。

source checkout からの local install は `cargo run --locked -p xtask -- install --locked` を公式入口とする。この command は `target/decune-xtask/container-tools-bundle` に container-side tools bundle を build/check し、bundle を埋め込んだ `decune` を `cargo install --path . --profile dist --bin decune` で install する。container-side tools bundle を埋め込まない build は正式な install 手順ではない。

`decune --version` は release tag から作る公式 artifact では `decune {version}` を表示する。source checkout からの local build では、tag 外 commit や dirty worktree を公式 artifact と区別できるように SemVer build metadata suffix を表示してよい。Git 情報を取得できない source build では source build であることを示す suffix を表示してよい。

開発・debug 用 override として `DECUNE_CONTAINER_TOOLS_DIR` を残す。build-time の bundle 制御は通常 `xtask` が内部で行い、bundle dir の既定値は `target/decune-xtask/container-tools-bundle` とする。`DECUNE_CONTAINER_TOOLS_BUNDLE` と `DECUNE_CONTAINER_TOOLS_BUNDLE_DIR` は低レベル build 用の内部 override として扱い、通常の local/CI 手順では利用者に要求しない。

## CLI

共通形式:

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

- `WORKSPACE` の既定値はカレントディレクトリ。
- `WORKSPACE` は実在するディレクトリでなければならない。
- Git repository 内では repository root を workspace root とする。Git repository でなければ指定ディレクトリを workspace root とする。
- `devcontainer.json` を必須とする。decune TOML は overlay であり、base image/build/Compose 定義の置き換えには使わない。
- CLI output、log、error message は英語にする。
- 設定変更が既存 container/project に反映できない場合、`up` は暗黙 rebuild を行わず、`Run decune rebuild` を促して終了する。

### `up`

```text
decune up [OPTIONS] [WORKSPACE]
```

役割:

- Development container を作成または起動し、remote user の shell にログインする。
- image/Dockerfile モードでは単一 container を作成または起動する。
- Compose モードでは Compose project を作成または起動し、primary service container にログインする。
- 既に起動済みで config hash が一致する場合、作成処理をスキップし、shell ログインのみ行う。
- decune host daemon、credential bridge、port forwarder は `up` process が生きている間だけ動作する。

主なオプション:

- `--config <PATH>`: `devcontainer.json` を明示する。relative path は workspace root 相対。
- `--detach`: shell に接続せず起動だけ行う。
- `--rebuild`: 既存 container/project を破棄または再作成する。decune が管理する volume は保持する。
- `--no-cache`: Dockerfile build、Compose service build、Feature layer build で cache を使わない。
- `--pull`: base image または Compose service image を pull してから build/create する。Compose モードでは config hash が一致する running container でも reuse fast path に入らず、pulled image を反映するため `docker compose up -d --force-recreate` まで進む。
- `--no-global-config`: global decune config を適用しない。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `-p, --port <SPEC>`: manual forwarding。例: `3000`, `3000/tcp`, `3000:3000`, `127.0.0.1:8080:3000`, `[::1]:8080:3000`。複数指定可。protocol suffix なしは TCP、`/tcp` は許可、`/udp` は unsupported error。Compose モードで service を指定したい場合は devcontainer `forwardPorts` の `"service:port"` を使う。

`--detach` では `up` process 終了時に host daemon も停止するため、manual/automatic forwarding と Git HTTPS host-helper は維持されない。detached container で外部公開が必要な port は、image/Dockerfile モードでは `appPort`、Compose モードでは Compose file の `ports` を使う。`--detach` と CLI `-p` / `--port` の併用は error とする。設定由来の `forwardPorts` / `[[ports]]` は warning を出して無視する。

### `rebuild`

```text
decune rebuild [OPTIONS] [WORKSPACE]
```

`up --rebuild` と同等の明示サブコマンドである。既存 container/project を停止・削除または force recreate し、再 build/create/start する。decune が管理する volume は保持する。

主なオプション:

- `--detach`
- `--no-cache`
- `--pull`
- `--update-features`: feature lock より registry/tag の再解決を優先する。
- `--no-global-config`: global decune config を適用しない。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `-p, --port <SPEC>`

Compose モードでは、`docker compose build` と `docker compose up -d --force-recreate` を使う。`--no-cache` は Compose service build と Feature layer build の両方に適用する。`--pull` は Compose service build/pull に適用するが、decune generated local image を親にする Feature / UID/GID / entrypoint shim layer build には適用しない。

### `down`

```text
decune down [--timeout <SECONDS>] [WORKSPACE]
```

- image/Dockerfile モード: decune が管理する container を停止する。volume、state、image は削除しない。
- Compose モード: decune が管理する Compose project を停止する。volume、state、image は削除しない。`runServices` 指定時も、Compose が `depends_on` 等で起動した dependency service を残さないよう project 全体を停止対象にする。
- Compose モードで現在の `devcontainer.json` / `dockerComposeFile` が削除、移動、または service rename 等で既存 resource と一致しない場合も、state または Docker label から decune が管理する Compose project を特定して停止する。
- 現在の設定が Compose モードでも、同じ workspace に過去の image/Dockerfile モード由来で decune が管理する container が残っている場合は停止する。

明示的な `decune down` は `shutdownAction` に関係なく停止を行う。

### `ports`

```text
decune ports [--json] [WORKSPACE]
```

役割:

- 実行中の attached `up` process が維持している port forwarding の対応関係を表示する。
- Docker published port は表示しない。image/Dockerfile モードの `appPort` と Compose file の `ports` は対象外である。
- 現在有効な forwarding がない場合も success とし、通常出力は `No active forwarded ports`、JSON 出力は `[]` とする。

通常出力:

- `LOCAL`: 実際に listen している host 側 endpoint。
- `TARGET`: 転送先。primary service は `container:<port>/<protocol>`、sidecar service は `<service>:<port>/<protocol>`。
- `SOURCE`: `configured` または `auto`。
- `REQUESTED`: 要求 host port と実 host port が異なる場合だけ要求 endpoint。異ならない場合は `-`。
- `LABEL`: port label。未指定なら `-`。

`--json` は `host_ip`、`host_port`、`requested_host_port`、`service`、`container_port`、`protocol`、`source`、`label` を持つ JSON array を stdout に出力する。

### `remove` / `rm`

```text
decune remove [--no-confirm] [--images] [WORKSPACE]
decune rm     [--no-confirm] [--images] [WORKSPACE]
decune remove [--no-confirm] [--images] --all-workspaces
decune rm     [--no-confirm] [--images] --all-workspaces
```

- image/Dockerfile モード: decune が管理する container、decune が管理する volume、state/runtime を削除する。`--images` 指定時だけ generated image を削除する。
- Compose モード: decune が管理する Compose project を `docker compose down --volumes --remove-orphans` 相当で削除し、state/runtime を削除する。external volume/network は Compose の標準挙動に従い削除しない。`--images` 指定時だけ decune generated image を削除する。user が Compose file で指定した image を `--rmi all` で削除してはならない。
- Compose モードで現在の `devcontainer.json` / `dockerComposeFile` が削除、移動、または service rename 等で既存 resource と一致しない場合も、state または Docker label から decune が管理する Compose project を特定して削除する。
- 現在の設定が Compose モードでも、同じ workspace に過去の image/Dockerfile モード由来で decune が管理する container や volume が残っている場合は削除する。
- `--all-workspaces` は、すべての workspace で decune が管理する Dev Container 環境を削除する。`WORKSPACE` とは排他である。
- `--all-workspaces` の探索対象は `decune.managed=true` と `decune.workspace_id` を持つ Docker container / volume、および `$XDG_STATE_HOME/decune/*/state.toml` の有効な state file とする。読み込めない state file は warning を出して無視する。
- `--all-workspaces` で Compose project を削除する場合は、decune が管理する container の `com.docker.compose.project` label または decune state の `compose_project_name` から所有を確認できる project だけを対象にする。project name prefix だけでは user が管理する Compose project を対象にしない。
- `--all-workspaces` は対象 workspace の state/runtime を削除する。workspace cache と共有 Feature archive cache は削除しない。

`rm` は `remove` の alias とする。`--no-confirm` は確認プロンプトだけを省略し、decune が管理するリソースだけを対象にする安全境界や使用中のリソースの保護は迂回しない。

削除対象がある状態で TTY でない環境から `remove` を `--no-confirm` なしで実行した場合は、確認不能として error にする。`--all-workspaces` で削除対象が 0 件の場合は、TTY でない環境でも確認せず success とする。

## devcontainer.json サポート

### 検出順序

workspace root から以下の順で検出する。

1. `.devcontainer/devcontainer.json`
2. `.devcontainer.json`
3. `.devcontainer/<name>/devcontainer.json`

`--config <PATH>` が指定された場合は自動検出を行わず、その path を `devcontainer.json` として使う。relative path は workspace root 相対で解決する。3 に複数候補がある場合は自動選択せず、`--config .devcontainer/<name>/devcontainer.json` で明示する。

### 構成モードの判定

| mode | 必須 property | 禁止 property | 備考 |
| --- | --- | --- | --- |
| image | `image` | `build`, `dockerComposeFile`, `service` | image を pull して container を作る |
| Dockerfile | `build.dockerfile` | `image`, `dockerComposeFile`, `service` | Dockerfile を build して container を作る |
| Docker Compose | `dockerComposeFile`, `service` | `image`, `build` | Compose が image/build を持つ |

`dockerComposeFile` と `service` は片方だけ指定してはならない。`runServices` は Compose モード専用であり、指定する場合は `dockerComposeFile` と `service` も必須である。

### 対応プロパティ

| property | image | Dockerfile | Compose | 備考 |
| --- | --- | --- | --- | --- |
| `image` | yes | no | no | image-based mode |
| `build.dockerfile` | no | yes | no | Dockerfile-based モード |
| `build.context` | no | yes | no | `devcontainer.json` からの相対 path |
| `build.args` | no | yes | no | string value のみ |
| `build.options` | no | partial | no | Docker build argv に渡す。decune が管理する option と context path は不可 |
| `build.target` | no | yes | no | multi-stage build target |
| `build.cacheFrom` | no | partial | no | Docker CLI で扱える形式 |
| `dockerComposeFile` | no | no | yes | string / string array。local path のみ |
| `service` | no | no | yes | primary service |
| `runServices` | no | no | yes | 未指定時は全 service。primary service は常に含める |
| `features` | yes | yes | yes | Compose モードは primary service final image に適用 |
| `overrideFeatureInstallOrder` | yes | yes | yes | Feature install order に反映 |
| `overrideCommand` | yes | yes | yes | image/Dockerfile 既定 true、Compose 既定 false |
| `mounts` | partial | partial | partial | bind/volume 対応。Compose モードは primary service に override として追加。tmpfs は error |
| `workspaceMount` | yes | yes | no | Compose モードは unsupported error。Compose file の primary service `volumes` を使う |
| `workspaceFolder` | yes | yes | yes | Compose モードの既定は `/` |
| `containerEnv` | yes | yes | yes | Compose モードは primary service `environment` override。secret storage ではない |
| `remoteEnv` | yes | yes | yes | exec/lifecycle/shell に適用。`${localEnv:...}` 由来 value は argv/log redaction 対象 |
| `remoteUser` | yes | yes | yes | shell/lifecycle user |
| `containerUser` | yes | yes | yes | Compose モードは primary service `user` override |
| `updateRemoteUserUID` | yes | yes | yes | Linux host で既定 true |
| `userEnvProbe` | yes | yes | yes | `none`, `loginShell`, `interactiveShell`, `loginInteractiveShell` |
| `forwardPorts` | yes | yes | yes | TCP-only。protocol suffix なしは TCP、`/tcp` は許可、`/udp` は unsupported error。Compose モードは `"service:port"` を受け付ける |
| `portsAttributes` | partial | partial | partial | `label`, `onAutoForward`, `requireLocalPort`。`protocol`, `elevateIfNeeded` は warning して無視 |
| `otherPortsAttributes` | partial | partial | partial | automatic forwarding の既定。unsupported fields は warning |
| `appPort` | yes | yes | no | TCP-only。protocol suffix なしは TCP、`/tcp` は許可、`/udp` は unsupported error。Compose モードは unsupported error。Compose file の service `ports` を使う |
| `runArgs` | partial | partial | no | Compose モードは unsupported error。Compose file の service attributes を使う |
| `init` | yes | yes | yes | Compose モードは primary service `init` override |
| `privileged` | yes | yes | yes | Compose モードは primary service `privileged` override |
| `capAdd` | yes | yes | yes | Compose モードは primary service `cap_add` override |
| `securityOpt` | yes | yes | yes | Compose モードは primary service `security_opt` override |
| lifecycle commands | yes | yes | yes | Feature metadata 由来 command は user command より前に実行 |
| `waitFor` | partial | partial | partial | parse するが attached `up` は `postAttachCommand` まで同期実行 |
| `name` | ignored | ignored | ignored | runtime behavior には使わない |
| `shutdownAction` | partial | partial | partial | attached `up` 終了時に適用。明示 `down` / `remove` が正 |
| `hostRequirements` | ignored | ignored | ignored | warning |
| `customizations` | ignored | ignored | ignored | preserve するが実行しない |

### JSONC

`devcontainer.json` は JSON with Comments として扱う。コメント除去を正規表現で実装しない。`//` line comment、`/* ... */` block comment、trailing comma は JSONC として受け付ける。

JSON5 全体はサポートしない。single-quoted string、unquoted key、hex number、`#` comment などの JSON5-only syntax は invalid metadata として扱う。

### `runArgs` 許可リスト

image/Dockerfile モードが受け付ける `runArgs` は以下のみ。

- `--init`
- `--privileged`
- `--cap-add <CAP>`
- `--security-opt <OPT>`
- `--add-host <HOST:IP>`
- `--dns <IP>`
- `--dns-search <DOMAIN>`
- `--network <NETWORK>`
- `--network-alias <ALIAS>`
- `--hostname <HOSTNAME>`
- `--device <HOST_PATH[:CONTAINER_PATH[:PERMISSIONS]]>`
- `--group-add <GROUP>`
- `--ulimit <NAME=SOFT[:HARD]>`
- `--ipc <MODE>`
- `--shm-size <SIZE>`
- `--gpus <REQUEST>`

value を取る option は `--foo=value` と `--foo value` の両方を受け付け、内部では `--foo`, `value` へ正規化する。`--init` と `--privileged` は value なしの boolean option としてのみ受け付ける。`--cap-add` と `--security-opt` は Dev Container の専用 field と同じ扱いで merge する。その他の許可 option は Docker create に `option value` として渡す。

上記以外は unsupported error とする。特に decune が container identity、environment、user/workdir、mount、published port、label、entrypoint、lifecycle/control を管理するため、`--name`、`--env` / `-e`、`--env-file`、`--user` / `-u`、`--workdir` / `-w`、`--mount`、`--volume` / `-v`、`--tmpfs`、`--volumes-from`、`--publish` / `-p`、`--publish-all` / `-P`、`--expose`、`--entrypoint`、`--label`、`--label-file`、`--rm`、`--detach` / `-d`、`--restart` は reserved option として拒否する。published port は `appPort` または Compose service `ports`、port forwarding は `forwardPorts` / decune `[[ports]]` / CLI `-p`、mount は `mounts`、user は `containerUser`、working directory は `workspaceFolder`、環境変数は `containerEnv` を使う。

Compose モードでは `runArgs` を unsupported error とする。Compose service の `init`、`privileged`、`cap_add`、`security_opt`、`extra_hosts`、`dns`、`dns_search`、`devices`、`network_mode`、`ports`、`volumes`、`user`、`environment` などを Compose file に書くか、Dev Container の cross-orchestrator property を使う。

### `workspaceMount` / `workspaceFolder`

image/Dockerfile モードでは、`workspaceMount` を明示する場合は `workspaceFolder` も明示必須とする。`workspaceFolder` は workspace mount target 配下でなければならない。`workspaceMount` 未指定時は `/workspaces/<localWorkspaceFolderBasename>` を bind mount target とし、`workspaceFolder` 未指定時はその target を working directory とする。

Compose モードでは `workspaceMount` は unsupported error とする。workspace の mount は Compose file の primary service `volumes` に定義する。`workspaceFolder` 未指定時の既定は `/` である。

## Docker Compose モード

### Compose モードの制限

Compose モードでは Compose service の runtime 設定を Docker Compose に委譲する。decune は以下の Dev Container properties を generated Compose override へ自動変換せず、metadata validation で unsupported error とする。

| Dev Container property | Compose モードの扱い | 代替 |
| --- | --- | --- |
| `workspaceMount` | unsupported error | workspace bind mount を primary service の `volumes` に書く |
| `appPort` | unsupported error | Docker published port 設定を Compose service の `ports` に書く |
| `runArgs` | unsupported error | `init`、`privileged`、`cap_add`、`security_opt`、`extra_hosts`、`dns`、`dns_search`、`devices`、`network_mode` など Compose service の field に書く |

Docker published port 設定は Compose file に委譲する。Compose モードで外部公開が必要な port は Compose service の `ports` を使い、decune port forwarding は `forwardPorts`、decune `[[ports]]`、CLI `-p` を使う。

Compose モードでも decune は、対応している cross-orchestrator properties と runtime 機能を primary service または primary service container に適用する。対象は `containerEnv`、`remoteEnv`、`containerUser`、`remoteUser`、`init`、`privileged`、`capAdd`、`securityOpt`、`mounts`、dotfiles mount、credentials/runtime mount、lifecycle command、remote shell、automatic forwarding である。`remoteEnv` は primary service container で実行する lifecycle command、hook、remote shell に適用する。

### Compose file の解決

`dockerComposeFile` は string または string array である。各 path は `devcontainer.json` のある directory から相対解決する。絶対 path は portable でないため warning 対象とする。path escape は許可するが、state/hash には canonical path と file digest を含める。存在しない path は error とする。

解決した Compose file は指定順に `docker compose -f <file>` へ渡す。後続 file が前 file を override/add する Compose 標準の merge semantics に従う。relative path resolution の基準は Docker Compose CLI の標準挙動に合わせ、第一 Compose file の parent directory を project directory とする。必要に応じて `--project-directory <first-compose-file-parent>` を明示する。Docker Compose child process の current directory も project directory に固定し、Compose interpolation の `.env` 解決が decune 呼び出し元 PWD ではなく Compose project directory 基準になるようにする。第一 Compose file が symlink の場合、project directory は final symlink を辿った canonical path の parent ではなく、`devcontainer.json` 相対で解決した入力 path の parent とする。

`dockerComposeFile` から git URL、OCI artifact、stdin を参照する構成は unsupported error とする。

### Compose project name

decune は Compose project name を必ず明示する。top-level `name:`、`COMPOSE_PROJECT_NAME`、current directory basename に依存しない。

```text
decune-<safe_workspace_slug>-<workspace_id>
```

- lowercase ASCII、decimal digits、dash のみ。
- 先頭は `decune-` 固定。
- `workspace_id = hex(sha256(canonical_workspace_path))[0..12]`。
- config hash は project name に含めない。同じ workspace の rebuild で project name は安定する。

Compose CLI には `--project-name <project>` を渡す。`COMPOSE_PROJECT_NAME` が host env に存在しても、CLI option を優先する。

### Compose 正規化と検証

Compose モードの計画作成時、decune は以下を実行する。

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

Compose モードで decune 固有機能を適用するため、state/runtime directory に generated override file を作る。この file は user が編集しない。

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
- `down` / attached `up` 終了時の `stopCompose` は、`runServices` の service 引数で対象を狭めず、Compose project 全体を停止する。これは Compose が `depends_on` 等で暗黙に起動した dependency service を残さないためである。`remove` は project 全体を削除対象にする。

### Build / pull / recreate

Compose モードの image creation は次の順で行う。

1. `initializeCommand` を host で実行する。
2. user Compose file だけで `docker compose config --format json` を実行し、primary service の base image/build 情報を検証する。
3. `docker compose build` または `docker compose up -d --build` で primary service と必要な service image を準備する。`--no-cache` と `--pull` は Compose build option に反映する。
4. primary service の base image を特定する。Compose service に `build` がある場合は Compose が tag した service image を使う。`image` がない build-only service では Compose の既定 tag `<project-name>-<service>` を使う。service に `image` のみがある場合はその image を使い、metadata 解決前に missing image を pull する。
5. Feature、UID/GID sync、entrypoint shim が必要な場合、base image に decune generated layer を重ね、decune generated image tag を作る。
6. generated Compose override に primary service image 差し替えを反映する。decune generated local image に差し替える場合は `pull_policy: never` も反映する。
7. generated override 込みで `docker compose up -d` を実行する。`--pull` または `rebuild` の場合は `--force-recreate` を渡す。
8. `docker compose ps --format json` と `docker inspect` で primary container ID を解決し、lifecycle と shell attach に進む。

`--pull` は user Dockerfile build、base image pull、Compose service build/pull にだけ適用する。Feature、UID/GID sync、entrypoint shim などの decune generated layer は直前に準備した local image tag を `FROM` にすることがあるため、これらの layer build には Docker build の `--pull` を渡さない。

Dockerfile-based モードの `build.options` は、Docker build の context 引数 `-` より前に argv として渡す。shell 文字列には連結しない。decune が管理する `-f` / `--file`、`-t` / `--tag`、`--label`、`--build-arg`、`--target`、`--cache-from`、`--no-cache`、`--pull`、`--rm` / `--force-rm`、`--iidfile`、`--metadata-file`、`--output` などの option は `build.options` では指定できない。`build.options` は option だけを受け付け、build context path は decune が stdin tar と最後の `-` で管理する。

`--platform`、`--ssh`、`--secret`、`--add-host`、`--network` など Docker CLI に委譲できる build option は指定できる。ただし `build.options` の値は argv に出るため、secret 文字列そのものを直接書かない。secret は `--secret id=npm,env=NPM_TOKEN` のように host 環境変数や file path を参照する形にする。

`rebuild` は generated image と Compose service を再作成する。anonymous volume は保持する。`remove --images` 以外で user image や Compose service image を削除してはならない。

### shutdownAction

Dev Container の既定値に合わせる。

- image/Dockerfile モードの既定: `stopContainer`
- Compose モードの既定: `stopCompose`

attached `up` で shell が終了したとき:

- `none`: container/project を停止しない。
- `stopContainer`: primary container だけ停止する。
- `stopCompose`: Compose モードでは Compose project 全体を停止する。image/Dockerfile モードでは `stopContainer` と同じ。

明示的な `decune down` / `decune remove` は user 操作として扱い、`shutdownAction` によって no-op にはしない。

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
7. CLI options

`decune up --no-global-config` / `decune rebuild --no-global-config`、または project config の `use_global_config = false` を指定した場合、4 の global decune config は読み込まず、合成対象にも含めない。global config を読み込まないため、global config file の parse / validation error も発生しない。CLI option は一時的な強制無効化として扱い、project config で再有効化できない。

`--config <PATH>` は `devcontainer.json` を選択するだけであり、decune TOML overlay の追加指定ではない。

### merge rule

- scalar: 後勝ち。
- `init` / `privileged`: boolean scalar として後勝ち。上位 layer の `false` は下位 layer の `true` を打ち消せる。
- `capAdd` / `securityOpt`: security list として deduped union。
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
use_global_config = true
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

### トップレベル

- `version`: 必須。`1` のみ。
- `use_global_config`: 任意。既定 true。project config で false にすると global decune config を適用しない。
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
- `resolve_symlink`: 既定 true。true の場合は source を canonicalize する。file の場合は canonicalized source を直接 bind mount する。directory の場合は、配下 symlink がなければ canonicalized source を直接 bind mount する。配下 symlink があり、同一 backing root に完全一致する場合は backing root を直接 bind mount する。完全一致しない場合は state dir に mount 用 skeleton を作成し、skeleton と symlink 解決後の実ファイル/実ディレクトリを追加 bind mount する。skeleton と追加 bind mount の writable/read-only は `read_only` に従う。`read_only = false` の skeleton-only path に container から新規作成された file/directory は、元 source ではなく state dir の skeleton に保存され、以後の skeleton 準備でも保持される。`read_only = true` の skeleton では現在の dotfile tree に不要な stale entry を削除するが、既存 container の running reuse では skeleton を再生成しない。dotfile 内容は state dir にコピーしない。broken symlink、循環 symlink、特殊ファイル、mount 数過多など直接 bind mount として表現できない場合は error。
- `on_conflict`: `fail`, `replace-symlink`, `backup`。既定 `fail`。

Compose モードでは primary service に dotfiles bind mount と setup lifecycle を適用する。

### `[[mounts]]`

任意の追加 mount。

- `type`: `bind`, `volume`, `tmpfs`。`bind` と `volume` に対応し、`tmpfs` は error。
- `source`: `bind` では必須。`volume` では volume 名。
- `target`: container absolute path。`/opt/decune` と `/run/decune` 配下、および workspace mount target と同一 target は禁止。
- `enabled`: 既定 true。false の場合は同一 target を無効化。
- `read_only`: 既定 false。
- `resolve_symlink`: bind source にのみ適用。既定 true。
- `create`: `false`, `"directory"`。既定 false。file の自動作成は行わない。

Compose モードでは primary service に generated override として追加する。

### `[[ports]]`

manual port forwarding 設定。Docker published port ではない。

- `container`: container 側 port。必須。
- `host`: host 側 port。省略時は `container` と同じ番号を試し、占有済みなら空き port を探索する。
- `host_ip`: 既定 `127.0.0.1`。`0.0.0.0` は明示された場合のみ許可。
- `protocol`: `tcp` のみ。省略時も TCP。`udp` は unsupported error。
- `service`: Compose モードで対象 service を指定する任意 field。未指定は primary service。image/Dockerfile モードでは指定不可。
- `enabled`: 既定 true。
- `require_local`: true の場合、要求した host port と異なる port に fallback したら warning する。
- `label`: 表示用。

### `[ports.auto]`

- `enabled`: 既定 true。
- `min`: 既定 1024。
- `max`: 既定 32768。
- `ignore`: automatic forwarding から除外する port。
- `on_auto_forward`: `notify`, `silent`, `ignore`。browser/preview 系は CLI では `notify` 相当。

Compose モードの automatic forwarding は primary service の container を対象にする。sidecar service は明示 `forwardPorts` または `[[ports]].service` で指定する。

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
- `https`: `off`, `host-helper`, `host-helper-read-only`。既定 `host-helper`。
- `ssh_agent`: `off`, `auto`, `required`。既定 `auto`。

`host-helper` は container 内に `git-credential-decune` を配置し、host daemon 経由で host の `git credential fill/approve/reject` を呼ぶ。helper は container OS/arch 用 artifact であり、host の `decune` binary をそのまま bind mount しない。

`host-helper-read-only` は同じ helper staging/mount を使うが、container からの credential lookup だけを許可する。Git credential `get` は host の `git credential fill` に forwarding し、`store` / `erase` は host の `approve` / `reject` に渡さず success no-op として空出力を返す。untrusted repository では host credential store の mutation を避けるため、`host-helper-read-only` または `off` を推奨する。`host-helper-read-only` は SSH agent forwarding を変更しないため、SSH agent が不要な場合は `ssh_agent = "off"` も設定する。

`https = "off"` または `enabled = false` の場合、host daemon は Git credential request を host の Git credential helper に渡してはならない。

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

少なくとも `build.args` の value、`build.target`、`build.cacheFrom`、`workspaceFolder`、`containerEnv`、`remoteEnv`、`remoteUser`、`containerUser`、`mounts`、dotfiles、`runArgs` の value 部分で変数展開する。`workspaceFolder` は変数展開後に absolute path validation を行う。`workspaceFolder` 内の `${containerWorkspaceFolder}` は default workspace folder を基準に展開する。`workspaceFolder` 未指定時に decune が合成する default workspace folder は設定 string value ではないため、変数展開せず literal path として扱う。lifecycle command 本体、`dockerComposeFile`、`service`、`runServices`、`forwardPorts`、`appPort` の追加変数展開は行わない。

`build.args`、`build.target`、`build.cacheFrom` は Dockerfile build 前に展開するため、最終 image や runtime container からしか分からない値には依存できない。これらの field で `${remoteUserHome}` を使う構成は error とする。`${remoteUser}` は `remoteUser` または `containerUser` が config / metadata から build 前に決まる場合だけ使える。Dockerfile `USER`、Compose service `user`、image config `User` 由来の user は build 前の `build.*` 変数展開には使わない。

`${remoteUserHome}` は `/home/<user>` と推測せず、container/image 内の passwd database から解決する。`workspaceFolder`、`containerEnv`、`remoteEnv`、`mounts`、dotfiles、`runArgs` など runtime user 解決後に評価できる field では、effective remote user 決定後に `${remoteUser}` / `${remoteUserHome}` を展開する。`containerEnv` 自体の中で `${containerEnv:...}` を使う構成は error とする。

`${localEnv:...}` から展開された `containerEnv` / `remoteEnv` / `build.args` value は secret-sensitive として追跡する。decune はその実値を state、config hash、generated Compose override、Docker/Compose label、argv、通常の error log に平文保存してはならない。config hash では key を保持し、`containerEnv` と `build.args` は変更検出のため実値ではなく非可逆 digest を含め、`remoteEnv` は redacted marker に置き換える。Compose モードの generated override では primary service `environment` に `${DECUNE_CONTAINER_ENV_<SAFE_KEY>}` 形式の placeholder を書き、実値は `docker compose` child process の environment として渡す。placeholder variable name の `<SAFE_KEY>` は `containerEnv` key から ASCII alphanumeric / underscore のみへ正規化した値とする。Docker build args は process environment と `--build-arg KEY` で Docker CLI に渡し、argv に value を直接載せない。

`containerEnv` は container 作成時の環境変数であり、container 内プロセスや Docker inspect から見える。`build.args` は Docker build に渡り image layer や build output に残る可能性がある。`runArgs`、`workspaceFolder`、`remoteUser`、`containerUser`、`build.target`、`build.cacheFrom` は command、state、label、container identity に出る可能性がある。decune はこれらを secret storage として保証しない。literal に書かれた secret 文字列や、decune が `${localEnv:...}` 由来と追跡できない値は自動では secret-sensitive と判定しない。build secret には Docker BuildKit secret を使う。

host bind source の処理順:

1. `~` を展開。
2. `${...}` を展開。
3. relative path を基準 directory から absolute path にする。
4. `create = "directory"` なら directory を作成。
5. `resolve_symlink = true` なら canonicalize。
6. 存在しない path は `create` が指定されていない限り error。

Compose file 内の environment interpolation は Docker Compose CLI に委譲する。decune は `devcontainer.json` と decune TOML の値だけを自前で展開する。

## ランタイムアダプター

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

### 互換性

- Docker CLI は Docker デーモン と同じ host/remote context を指す。
- Compose CLI は Docker CLI と同じ `DOCKER_HOST` / `DOCKER_CONTEXT` / `DOCKER_CONFIG` を継承する。
- Podman 互換 endpoint は、Docker CLI / Compose CLI が透過的に扱える範囲でのみ対象にする。Podman Compose 固有挙動は公式対象外。

## Docker リソースと状態

workspace id:

```text
hex(sha256(canonical_workspace_path))[0..12]
```

image/Dockerfile モードの Docker resource name には workspace basename をそのまま使わず、ASCII safe slug と workspace id を組み合わせる。

- container: `decune-<safe_workspace_slug>-<workspace_id>`
- image: `decune/<safe_workspace_slug>-<workspace_id>:<config_hash>`
- state directory: `$XDG_STATE_HOME/decune/<workspace_id>` または `~/.local/state/decune/<workspace_id>`
- runtime directory: `$XDG_RUNTIME_DIR/decune/<workspace_id>` または `/tmp/decune-<uid>/<workspace_id>`

Compose モード:

- project: `decune-<safe_workspace_slug>-<workspace_id>`
- generated primary image: `decune/<safe_workspace_slug>-<workspace_id>:<config_hash>`
- generated Compose override: `$XDG_STATE_HOME/decune/<workspace_id>/compose.override.yaml`
- state/runtime directory は image/Dockerfile モードと同じ。

主な decune label:

- `decune.managed=true`
- `decune.workspace=<canonical_workspace_path>`
- `decune.workspace_id=<workspace_id>`
- `decune.config_hash=<hash>`
- `decune.version=<version>`
- `devcontainer.local_folder=<canonical_workspace_path>`
- `devcontainer.config_file=<path>`

Compose モードでは上記 label を primary service に追加する。明示的な sidecar service forwarding 対象 service には、forwarding runtime mount の再作成判定に必要な `decune.managed=true` と `decune.workspace_id=<workspace_id>` を追加する。Compose が付与する `com.docker.compose.project` と `com.docker.compose.service` も container identity に使う。`com.docker.compose.*` prefix を decune の generated override で上書きしてはならない。

既存 container/project の再利用は `decune.managed=true` と `decune.workspace_id` が一致するものに限る。他ツールの container は拾わない。

config hash には、resolved metadata/config、Feature lock、relevant CLI options、Dockerfile 内容、`build.options`、effective ignore file、build context digest、entrypoint plan、Linux host の UID/GID sync input、Compose モードの user Compose files から得た sanitized canonical Compose model、Compose file digest、generated override semantic hash input を含める。manual/automatic forwarding の現在値、credential token value、SSH agent socket path、GitHub token file path、`${localEnv:...}` 由来の `remoteEnv` value、Compose secrets の解決済み value は含めない。`${localEnv:...}` 由来の `containerEnv` value は平文では含めず、container 作成時環境の変更を検出するため非可逆 digest として含める。Compose モードでは user Compose files だけを対象にした `docker compose config --format json` が解決した interpolation / env file / profile / merge 結果から、`services.<service>.environment` の leaf value を平文ではなく digest marker に置き換えた canonical Compose model を hash に含める。この digest input は `decune-compose-env-value-hash-v1` で domain-separated / versioned にし、JSON path、JSON value type、canonical JSON value を含める。digest marker は `decune-compose-env-value-hash-v1:sha256:<hex>` 形式とし、environment value の平文を state、label、log、config hash input に残してはならない。generated override semantic hash input には primary service、decune が追加する label / environment / mount / user / security option / startup command、および decune generated image へ差し替えるかどうかを含める。`${localEnv:...}` 由来の `containerEnv` value は redacted marker または placeholder として扱い、実値を content hash 入力にしない。ただし generated override 内の `decune.config_hash` label や hash 由来 image tag など、hash 自身から派生する値は循環を避けるため hash 入力にしない。

state file は `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` に保存する。write は atomic に行う。Docker/Compose label と state が矛盾する場合、container/project identity と config hash は runtime label を正とする。lifecycle 完了 marker と `devcontainer.json` path は state に記録し、creation lifecycle の二重実行や `up --config` 後の Compose project lifecycle 復元に使う。

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
4. Dockerfile build 結果 image の `devcontainer.metadata` label を読み、image metadata layer として `devcontainer.json` や decune TOML と merge する。
5. Dockerfile build 結果 image に Feature を重ねる。
6. 必要なら UID/GID sync layer と entrypoint shim layer を重ねる。

Known limitations:

- Dockerfile が build context 外にある構成を unsupported error とする。decune は build context tar を生成して `docker build -` に渡すため、`--file` は tar 内の path を指す必要がある。このため `build.dockerfile` は解決後の `build.context` 配下に存在しなければならない。回避策は、`build.context` を Dockerfile を含む上位 directory に広げるか、Dockerfile を context 内へ移動することである。将来互換性を上げる場合は、context 外 Dockerfile を synthetic tar entry として追加し、Dockerfile-specific ignore file と context digest の semantics を Docker CLI と揃える必要がある。
- Dockerfile build 後に判明する `devcontainer.metadata` label は build 入力には使わない。このため `build.args`、`build.target`、`build.cacheFrom` の `${remoteUser}` は、`devcontainer.json` や decune TOML など build 前に解決できる `remoteUser` / `containerUser` だけを参照できる。

### Docker Compose-based

Compose primary service の image/build を base image として扱う。Feature は primary service の final image にだけ適用する。sidecar service には Feature、UID/GID sync、entrypoint shim、dotfiles、credentials を自動適用しない。

primary service に `build` がある場合、まず Compose CLI で service image を build する。primary service に `image` のみがある場合、必要に応じて pull する。base image 解決後、image/Dockerfile モードと同じ Feature/UID/GID/entrypoint layer pipeline を適用し、generated Compose override で primary service image を final image に差し替える。

Feature:

- OCI registry ref と local `./` ref に対応する。
- direct HTTPS tgz Feature は未対応。
- registry auth は Docker CLI 互換で `credHelpers`、`credsStore`、`auths` の順に source を選ぶ。選択 source が失敗しても別 source に fallback しない。
- manifest body と layer blob は sha256 digest を検証する。
- local Feature path は `devcontainer.json` directory からの相対 `./` path に限定し、absolute path と path escape を拒否する。
- local Feature directory basename と `devcontainer-feature.json` の `id` は一致必須。
- `devcontainer-feature.json` と `install.sh` は必須。
- OCI Feature は `<workspace>/.decune/features.lock.toml` に digest lock を記録する。
- `rebuild --update-features` は lock より再解決を優先する。
- Feature metadata は required field `id`, `version`, `name` を要求する。
- `installsAfter` は soft dependency として扱い、install worklist に存在しない Feature を追加しない。仕様上は version tag / digest を含められないが、互換性のため matching 用には tag / digest を落とした canonical Feature ID として扱う。
- Feature option は Features 仕様に従って env key に変換し、default option も export する。env key collision は error。
- Feature metadata の `containerEnv` は、Feature layer Dockerfile の `ENV` として各 Feature の `install.sh` 実行前に適用し、後続 Feature と最終 image に継承する。`PATH="/tool:${PATH}"` のような Dockerfile environment replacement は Docker builder に委譲する。Feature 由来 `containerEnv` は container create / generated Compose override の `environment` には再投入せず、user/devcontainer/project 由来の `containerEnv` だけを runtime override として適用する。

## コンテナの作成・起動と user

image/Dockerfile モードでは、workspace mount 未指定時は `/workspaces/<localWorkspaceFolderBasename>` へ bind mount する。

Compose モードでは workspace mount を自動追加しない。primary service の Compose `volumes` に workspace bind mount がない場合でも decune は起動を続けるが、`workspaceFolder` が存在しない場合は lifecycle/shell 実行前に error とする。

user 解決:

- effective container user: `containerUser`、image/Feature metadata `containerUser`、Compose service `user`、Docker image config `User`、`root`。
- effective remote user: `remoteUser`、image/Feature metadata `remoteUser`、effective container user。

存在しない effective remote user は root fallback せず configuration error とする。numeric UID/GID は passwd entry がなくても runtime identity として扱えるが、home directory が必要な処理では error または warning skip になる。

`updateRemoteUserUID` は Linux host で既定 true。remote user が明示されていれば remote user、なければ `containerUser`、image/Feature metadata `containerUser`、Compose service `user` のいずれかで container user が明示されている場合に container user を sync target とする。非 Linux host、root target、`updateRemoteUserUID = false`、passwd entry がない numeric target は no-op または warning skip とする。

Compose モードで UID/GID sync が必要な場合、primary service base image に sync layer を重ねた final image を作る。running container 内で `/etc/passwd` を直接 mutation しない。
UID/GID sync によって runtime user 表現が変わる場合、generated Compose override の primary service `user` には sync 後の user/group を反映し、元の numeric UID/GID で primary process を起動しない。

## Lifecycle とシェル接続

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

non-detach `up` / `rebuild` は lifecycle 後に remote user shell を TTY attach し、shell exit code を CLI exit code として返す。shell attach は `docker exec` 相当の CLI adapter で primary container に対して実行する。Compose モードでも `docker compose exec` ではなく、container ID を解決して `docker exec` 相当を使ってよい。

`--detach` では attach lifecycle、forwarding listener、`postAttachCommand`、shell attach を実行しない。

## Git/GitHub 認証

### Git HTTPS

`[credentials.git].https = "host-helper"` の場合、container 内に `git-credential-decune` を配置し、Git credential helper として設定する。helper は host daemon に versioned JSON request を送り、host の `git credential fill/approve/reject` を実行する。

`[credentials.git].https = "host-helper-read-only"` の場合も container helper protocol は同じである。host daemon が policy を適用し、`get` は `fill` として実行する一方、`store` / `erase` は host credential store に伝播せず success no-op とする。

### SSH agent

`ssh_agent = "auto"` では host の `SSH_AUTH_SOCK` が Unix socket の場合のみ forwarding を設定する。container env の `SSH_AUTH_SOCK` は `/run/decune/ssh-agent.sock`。`ssh_agent = "required"` で socket が利用できない場合は error。

Compose モードでは SSH agent socket mount は primary service にのみ追加する。

### GitHub CLI

host の `gh auth token` が成功した場合、token を runtime directory に mode 0600 の file として作り、container には `/run/decune/secrets/github-token` として read-only mount する。`GH_CONFIG_DIR=/run/decune/gh` は writable ephemeral directory とする。token file は `up` 終了時に scrub し、`down` / `remove` で削除する。

Compose モードでは GitHub token file mount は primary service にのみ追加する。

## Port forwarding

`forwardPorts`、decune `[[ports]]`、CLI `-p` は port forwarding であり Docker published port ではない。host 側 listen address の既定は `127.0.0.1`。container 内で `127.0.0.1:<container port>` にだけ listen している process にも届くよう、container-side `decune-forward-agent` 経由で proxy する。

port forwarding と published port metadata は TCP-only とする。CLI `-p`、decune `[[ports]]`、Dev Container `forwardPorts`、Dev Container `appPort` は protocol suffix なしを TCP として扱い、`/tcp` は明示的な TCP 指定として受け付ける。`/udp` は unsupported error とし、UDP 対応は将来課題とする。

`appPort` は image/Dockerfile モードの Docker published port であり container create 時に決まる。host IP が指定されない場合、Docker の既定で全 interface に公開される可能性があるため warning 対象とする。`appPort` の published port metadata も TCP-only である。

CLI `-p` と Dev Container `appPort` の host IP は IPv4 / hostname / bracketed IPv6 を受け付ける。IPv6 host IP は `[::1]:8080:3000` のように bracketed form で指定し、内部 model では bracket なしで保持する。unbracketed IPv6 は colon 区切りと曖昧なため error とする。`forwardPorts` string の `[::1]:3000` は host IP `::1` への forwarding として扱い、`[::1]:8080:3000` のような host-port mapping は `forwardPorts` では unsupported error とする。

Compose モードでは Docker published port 設定は Compose file の `ports` に委譲する。`appPort` は unsupported error とする。

manual forwarding source priority:

1. CLI `-p`
2. project decune `[[ports]]`
3. devcontainer `forwardPorts`
4. global decune `[[ports]]`

host port が占有済みの場合、昇順で空き port を探索し、上限に達した場合は OS assigned port へ fallback する。`require_local = true` なら要求した host port と実際の forwarding port が異なる場合に warning し、false なら silent fallback する。空き確認後に別 process が port を取得した場合も、listener bind 時に再度 fallback する。

forwarding の host port reservation は IP family 境界を尊重する。IPv4 wildcard `0.0.0.0` は IPv4 address とだけ衝突し、IPv6 loopback / concrete address とは同一 host port を共有できる組み合わせとして扱う。同様に IPv6 wildcard `::` は IPv6 address とだけ衝突する。

Compose モードの service 解決:

- `forwardPorts` number: primary service の port。
- `forwardPorts` string `"3000"`: primary service の port。
- `forwardPorts` string `"db:5432"`: Compose service `db` の port。
- `portsAttributes` key `"db:5432"`: Compose service `db` の port attributes。
- `[[ports]].service = "db"`: Compose service `db` の port。

`forwardPorts` の `"service:port"` 形式と `[[ports]].service` は Compose モード専用である。image/Dockerfile モードでは service 名で対象 container を解決できないため unsupported error とする。

sidecar service forwarding は、その service の container ID を解決し、必要な container-side tool を runtime install して forward-agent を起動する。対象 service には forwarding runtime mount と decune identity label だけを generated override で追加し、credentials、dotfiles、GitHub token、SSH agent は自動注入しない。service の replica が 2 以上なら error とする。

automatic forwarding は TCP listening socket のみを対象にする。container agent が `/proc/net/tcp` と `/proc/net/tcp6` を読み、TCP LISTEN port を検出する。UDP socket は検出・転送しない。既定 scan interval は 2 秒、initial delay は 3 秒。manual forwarding 済みの port、Docker published port として扱われる port、ignore list、`portsAttributes.onAutoForward = "ignore"` は除外する。Compose モードの automatic forwarding は primary service のみを対象にする。

現在有効な forwarding の実効対応は `decune ports` で確認できる。`decune ports` は `decune up` process が runtime directory に公開する host-local status socket に問い合わせる。実効対応は `state.toml` には保存しない。stale metadata または接続不能な status socket は現在有効な forwarding ではないものとして無視する。

## Host daemon とセキュリティ境界

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
- Dev Container metadata と Compose file は bind mount、`privileged`、`capAdd`、`securityOpt`、published port、SSH agent forwarding、Git/GitHub credential forwarding により host や secret への強い到達性を container へ与え得る。
- GitHub token forwarding を有効にすると、container 内 process は token file にアクセスできる。
- untrusted repository では `.devcontainer/`、Compose file、local Feature を確認し、必要に応じて `[credentials.git].https = "host-helper-read-only"`、`[credentials.git].ssh_agent = "off"`、`[credentials.git].enabled = false`、`[credentials.github].enabled = false` を設定する。

`decune up` は、意図した設定どおりに動作する security surface については `Notice:` として表示する。設定が無視される、機能が縮退する、または補助処理の失敗から継続する場合は `Warning:` として表示する。

## 検証範囲

コントリビューター向けの検証コマンドは [development.md](development.md) に置く。この仕様の test coverage は、少なくとも以下の挙動グループを含める。

- image-based / Dockerfile-based / Docker Compose-based の `up` / `rebuild` / `down` / `remove`。
- Dockerfile build の入力、`.dockerignore` の扱い、`--no-cache`、`--pull`、未対応の Dockerfile/context 組み合わせ。
- Compose の `dockerComposeFile`、`service`、`runServices`、profiles、複数 file の merge、generated override の挙動、project cleanup の安全性。
- Feature 解決、lock の扱い、metadata merge、option env/default の扱い、local Feature の制約、UID/GID sync、entrypoint shim の挙動。
- dotfiles、mounts、lifecycle commands、hooks、shell attach、lifecycle の二重実行防止。
- manual/automatic port forwarding、published port の warning/error、sidecar 明示 forwarding、TCP-only の挙動。
- credential forwarding、token redaction、state repair、resource name の sanitization、secret leak regression coverage。
