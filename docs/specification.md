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
- `up`、`rebuild`、`down`、`status`、`remove` / `rm`、`ports` コマンド。
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
- Compose published port mapping/relocation で実際に host port または host IP を変更する場合、または clone isolation が `volumes_from` / `external_links` の container name 参照を書き換える場合は、generated override で Compose `!override` tag を使うため Docker Compose v2.24.4 以上が必要。
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

container-side tools は release build 時に host binary へ埋め込む。bundle は `git-credential-decune`、`decune-forward-agent`、`decune` の 3 tools を各 platform に 1 artifact ずつ持ち、初期 2 platform では 6 artifact を必須とする。container-side `decune` の Cargo binary target は `decune-container-cli`、bundle artifact name と container 内の user-facing command name は `decune` とする。Git repository には生成済み binary artifact を入れない。

container-side tool platform:

- `linux-amd64`
- `linux-arm64`

release asset は `SHA256SUMS` で検証できる。GitHub Actions release workflow は build provenance attestation を作成し、release publish 前に全 asset を draft release に添付する。

source checkout からの local install は `cargo run --locked -p xtask -- install --locked` を公式入口とする。この command は `target/decune-xtask/container-tools-bundle` に container-side tools bundle を build/check し、bundle を埋め込んだ `decune` を `cargo install --path . --profile dist --bin decune` で install する。container-side tools bundle を埋め込まない build は正式な install 手順ではない。

`decune --version` は release tag から作る公式 artifact では `decune {version}` を表示する。source checkout からの local build では、tag 外 commit や dirty worktree を公式 artifact と区別できるように SemVer build metadata suffix を表示してよい。Git 情報を取得できない source build では source build であることを示す suffix を表示してよい。

開発・debug 用 override として `DECUNE_CONTAINER_TOOLS_DIR` を残す。build-time の bundle 制御は通常 `xtask` が内部で行い、bundle dir の既定値は `target/decune-xtask/container-tools-bundle` とする。`DECUNE_CONTAINER_TOOLS_BUNDLE` と `DECUNE_CONTAINER_TOOLS_BUNDLE_DIR` は低レベル build 用の内部 override として扱い、通常の local/CI 手順では利用者に要求しない。

container-side tool の runtime staging は、container に mount する runtime directory 内へ temporary file を作らない。host-private かつ target と同一 filesystem の親 directory に排他的 create で temporary file を作り、開いた file descriptor への artifact bytes の書き込み、mode `0755` の設定、最終 staged bytes の SHA-256 検証が完了した後、runtime target を atomic rename で置換する。既存 target が symlink の場合は link 先を変更せず symlink entry 自体を置換し、directory など安全に置換できない file type は runtime corruption error とする。失敗時は temporary file を削除し、partial target を公開しない。

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
- `--automatic-published-port-relocation`: Compose automatic published port relocation policy をこの実行で有効化する。
- `--no-automatic-published-port-relocation`: Compose automatic published port relocation policy をこの実行で無効化する。
- `-p, --port <SPEC>`: manual forwarding。例: `3000`, `3000/tcp`, `3000:3000`, `127.0.0.1:8080:3000`, `[::1]:8080:3000`。複数指定可。protocol suffix なしは TCP、`/tcp` は許可、`/udp` は unsupported error。Compose モードで service を指定したい場合は devcontainer `forwardPorts` の `"service:port"` を使う。

automatic published port relocation policy は後続の Compose automatic published port relocation 処理が参照する設定である。既定は無効である。`--no-auto-forward` は automatic port forwarding だけを無効化し、automatic published port relocation policy は変更しない。

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
- `--automatic-published-port-relocation`
- `--no-automatic-published-port-relocation`
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

### `status`

```text
decune status [WORKSPACE]
```

役割:

- `WORKSPACE` なしでは、state file と decune が付けた Docker label から見つかる workspace environment の summary を表示する。
- `WORKSPACE` 指定時は、その workspace の detail を表示する。workspace は通常の workspace 解決と同じく Git repository root を workspace root とする。
- `status` は read-only command とする。state、runtime file、Docker resource を修復、削除、更新しない。`last_used_at` も更新しない。

summary:

- 対象は `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` の有効な state file、および `decune.managed=true` と有効な `decune.workspace_id` label を持つ Docker container / volume である。
- runtime directory や port-status directory だけが残っている workspace は summary 対象に含めない。
- 対象が 0 件の場合も success とし、`No decune-managed workspace environments found` を表示する。
- 1 件以上の場合は aggregate line と table を表示する。table column は `ID WORKSPACE RUNTIME CONFIG HEALTH FWD/PUB ISSUES LAST_USED` とする。
- sort は display workspace path の辞書順、tie-break は workspace id とする。workspace path が不明な entry は末尾に置く。
- `LAST_USED` は state の `last_used_at` だけから表示する。`created_at` や `last_started_at` へ fallback しない。値がない、invalid、future の場合は `-` とする。
- `FWD/PUB` は現在有効な forwarded port count と Docker published port count を `<forwarded>/<published>` 形式で表示する。

detail:

- `WORKSPACE` 指定時は devcontainer metadata を必須とする。metadata が見つからない、または複数候補がある場合は error にする。
- metadata があり state/Docker evidence がない場合は `not-created` として success し、`Run decune up to create the environment.` を action として表示する。
- detail は header (`Workspace`, `ID`, `Mode`) と、`Summary`、`Config`、issue がある場合の `Issues`、Compose mode の `Services`、`Runtime`、`Ports`、`Resources`、未完了 lifecycle がある場合の `Lifecycle`、必要な場合の `Action` を表示する。`Issues` は `code [severity]: message`、`Action` は action を持つ全 issue を `code: action` 形式で表示する。
- lifecycle が正常完了している場合は lifecycle step detail を表示しない。
- `Ports` section は `decune ports` の単一 workspace table と同じ形式を使う。active port がない場合は `No active ports for this workspace` を表示する。
- current config hash は、workspace path と config が読める場合に read-only で計算し、state または Docker label 由来の config hash と比較して `current` / `needs-rebuild` を判定する。`[[mounts]].create = "directory"` および Dev Container bind mount の `bind-create-src` は、missing host path を作成せず、既存 ancestor を canonicalize して missing tail を合成した path で hash を計算する。計算できない場合は `unreadable` または `unknown` issue として表示し、state、host path、Docker resource は変更しない。
- output には secret value、raw label、raw Compose model、container env、build arg、mount source の過剰な列挙、config hash 値を出してはならない。
- JSON 出力、`--ports`、`--resources` などの status option は提供しない。

### `ports`

```text
decune ports [--json] [WORKSPACE]
decune ports [--json] --all
```

役割:

- decune が管理している workspace について、現在有効な host 側 port の利用状況を表示する。
- 表示対象は、実行中の attached `up` process が維持している port forwarding と、Docker が現在 publish している port binding である。
- port forwarding は `forwardPorts`、decune `[[ports]]`、CLI `-p`、automatic forwarding を含む。
- Docker published port は image/Dockerfile モードの `appPort` と Compose service `ports` を含む。
- `--all` は decune が管理している workspace を横断して表示する。`--all` と `WORKSPACE` は同時指定できない。
- `ports` は read-only command とする。state、runtime file、Docker resource を修復、削除、更新しない。`last_used_at` も更新しない。
- 現在有効な host 側 port がない場合も success とし、通常出力は単一 workspace で `No active ports for this workspace`、`--all` で `No active ports`、JSON 出力は `[]` とする。

通常出力:

- `WORKSPACE`: `--all` の場合だけ表示する workspace path。不明なら `<unknown>`。
- `ID`: `--all` の場合だけ表示する workspace id。
- `LOCAL`: forwarding では実際に listen している host 側 endpoint。Docker published port では現在有効な host 側 endpoint を表示する。Compose published port relocation metadata がある published entry では、planned endpoint を通常出力向けの要約として表示する。host IP が省略された planned endpoint は `*:<port>` と表示し、Docker inspect で得た実際の binding は JSON の `actual_bindings` で確認できる。
- `TYPE`: `forwarded` または `published`。
- `TARGET`: 転送先、または Docker published port の container 側 endpoint。primary container は `container:<port>/<protocol>`、Compose service は `<service>:<port>/<protocol>`。
- `SOURCE`: forwarding は `configured` または `auto`、published port は `appPort` または `compose`。
- `REQUESTED`: port forwarding が要求 endpoint から別 endpoint へ fallback した場合、または Compose published port mapping/relocation により requested endpoint と planned endpoint が異なる場合に、要求 endpoint を表示する。それ以外は `-`。Compose published port で host IP が省略されている場合は `*:<port>` と表示し、explicit `0.0.0.0` と区別する。
- `STATE`: Compose published port mapping/relocation により requested endpoint と planned endpoint が異なる場合は `relocated`。host IP だけが異なる場合も含む。それ以外は `-`。
- `LABEL`: port label。未指定なら `-`。

`--json` は通常出力の table を再構成できる JSON array を stdout に出力する。

- 各 entry は `host_ip`、`host_port`、`type`、`service`、`container_port`、`protocol`、`source`、`label` を持つ。
- `--all` では `workspace` と `workspace_id` も含める。
- 要求 endpoint と実 endpoint が異なる forwarding entry では、`requested_host_ip` と `requested_host_port` を含める。
- decune が Compose published port relocation の metadata を保存している published entry では、`target`、`requested`、`planned`、`actual_bindings`、`relocated`、`port_entry_index` を含める。`target` は `port` と `protocol`、`requested` / `planned` は `host_ip` と `host_port` を持つ。`actual_bindings` は Docker inspect から得た現在の actual binding の配列で、各要素は `host_ip` と `host_port` を持つ。
- 同 published entry では既存 JSON consumer との互換のため、`requested_host_ip_kind`、`requested_host_port`、`planned_host_ip_kind`、`planned_host_port`、`relocated` も含める。
- 同 metadata の endpoint で host IP が明示されている場合は、`requested_host_ip` または `planned_host_ip` も含める。

`requested.host_ip` / `planned.host_ip` は host IP omitted の場合 `null`、explicit host IP の場合 string とする。`*_host_ip_kind` は `omitted` または `explicit` である。`omitted` は Compose file 上で host IP が省略されたことを表し、この場合、対応する flat `*_host_ip` は省略する。published port の requested endpoint は Docker が実際に publish している binding だけからは復元しない。

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
- `--all-workspaces` の探索対象は `decune.managed=true` と有効な `decune.workspace_id` を持つ Docker container / volume、および `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` の有効な state file とする。有効な workspace id は Docker label 由来・state directory 名由来のいずれも 12 桁の lowercase hex (`[0-9a-f]{12}`) に完全一致する値だけである。無効な label value や state directory 名は対象外として無視し、state/runtime path の組み立てに使わない。読み込めない state file は warning を出して無視する。
- `--all-workspaces` で Compose project を削除する場合は、decune が管理する container の `com.docker.compose.project` label または decune state の `compose_project_name` から所有を確認できる project だけを対象にする。project name prefix だけでは user が管理する Compose project を対象にしない。
- `--all-workspaces` は対象 workspace の state/runtime を削除する。workspace cache と共有 Feature archive cache は削除しない。

`rm` は `remove` の alias とする。`--no-confirm` は確認プロンプトだけを省略し、decune が管理するリソースだけを対象にする安全境界や使用中のリソースの保護は迂回しない。

削除対象がある状態で TTY でない環境から `remove` を `--no-confirm` なしで実行した場合は、確認不能として error にする。`--all-workspaces` で削除対象が 0 件の場合は、TTY でない環境でも確認せず success とする。

### `clean`

```text
decune clean [--dry-run] [--no-confirm] [--json]
decune clean --include-feature-cache [--dry-run] [--no-confirm] [--json]
```

`clean` は decune が管理している generated data を削除する maintenance command とする。Docker container、Compose project、Docker volume、Docker image、Docker builder cache、利用者が管理している filesystem は削除しない。`--all` と `--force` は提供しない。

既定の cleanup 対象は stale な workspace data だけである。

- `$XDG_CACHE_HOME/decune/<workspace_id>` または `~/.cache/decune/<workspace_id>`
- `$XDG_STATE_HOME/decune/<workspace_id>` または `~/.local/state/decune/<workspace_id>`
- `$XDG_RUNTIME_DIR/decune/<workspace_id>` または `/tmp/decune-<uid>/<workspace_id>`
- port forwarding status companion directory (`<runtime parent>/<workspace_id>-ports`)

workspace data は workspace id 単位で扱い、cache/state/runtime の一部だけを意図的に削除する mode は提供しない。有効な workspace id は 12 桁の lowercase hex (`[0-9a-f]{12}`) に完全一致する値だけである。無効な directory name や Docker label value は cleanup path の組み立てに使わない。

`--include-feature-cache` は既定の workspace data cleanup に共有 Feature archive cache (`$XDG_CACHE_HOME/decune/features` または `~/.cache/decune/features`) を追加する option とする。既定の `clean` は共有 Feature archive cache を削除しない。Feature archive cache の削除は Feature 取得・展開処理と同じ interprocess lock で保護し、`up` / `rebuild` と同時に archive cache を変更しない。

Safety model:

- 設定された XDG root と仕様で定義した fallback 配下の、decune が管理している path だけを探索する。
- symlink は辿らない。cleanup 対象自体または配下 entry に symlink がある対象は `unsafe_path` として skip する。
- decune が管理している root 外の path は削除しない。
- Docker label から `decune.managed=true` と有効な `decune.workspace_id` を持つ container / volume が見つかる workspace は decune が管理している再利用可能な resource とみなし skip する。
- runtime directory または port status directory 配下に接続可能な Unix socket、または取得できない lock file がある workspace は active とみなし skip する。
- Docker resource discovery に失敗した場合、削除実行は safety 判定不能として error にする。`--dry-run` では filesystem candidate を `docker_unavailable` として skip 表示できる。
- workspace file である `.decune/config.toml` と `.decune/features.lock.toml` は対象外である。
- runtime directory の file content は stdout/stderr、state、label、log に出してはならない。

TTY / non-TTY:

- TTY + `--no-confirm` なし + deletion candidate あり: summary を表示し、`[y/N]` で確認する。
- non-TTY + `--no-confirm` なし + deletion candidate あり: error にする。
- `--no-confirm`: 確認プロンプトだけを省略する。active / reusable workspace 保護や symlink refusal は迂回しない。
- `--dry-run`: 削除しないため確認不要。non-TTY でも実行できる。

`--json` は stdout に JSON object を出力する。root は `dry_run`、`include_feature_cache`、`summary`、`targets` を持つ。`summary` は `remove_candidates`、`removed`、`skipped` を持つ。workspace target は以下を持つ。

- `kind`: `"workspace"`
- `workspace_id`
- `action`: `"remove"` または `"skip"`
- `reason`: `"stale_workspace_data"`、`"managed_resource"`、`"active_runtime"`、`"unsafe_path"`、`"docker_unavailable"`、`"missing"` のいずれか
- `removed`: 実削除した場合だけ `true`
- `paths`: `cache`、`state`、`runtime`、`port_status`
- `existing_paths`: `"cache"`、`"state"`、`"runtime"`、`"port_status"` の array

Feature cache target は `kind = "feature_cache"`、`action`、`reason`、`removed`、`path` を持つ。Feature cache の `reason` は `"feature_cache_included"`、`"unsafe_path"`、`"missing"` のいずれかである。

## devcontainer.json サポート

### 検出順序

workspace root から以下の順で検出する。

1. `.devcontainer/devcontainer.json`
2. `.devcontainer.json`
3. `.devcontainer/<name>/devcontainer.json`

`--config <PATH>` が指定された場合は自動検出を行わず、その path を `devcontainer.json` として使う。relative path は workspace root 相対で解決する。3 に複数候補がある場合は自動選択せず、`--config .devcontainer/<name>/devcontainer.json` で明示する。

### 構成モードの判定

| mode           | 必須 property                  | 禁止 property                           | 備考                                      |
| -------------- | ------------------------------ | --------------------------------------- | ----------------------------------------- |
| image          | `image`                        | `build`, `dockerComposeFile`, `service` | image を pull して container を作る       |
| Dockerfile     | `build.dockerfile`             | `image`, `dockerComposeFile`, `service` | Dockerfile を build して container を作る |
| Docker Compose | `dockerComposeFile`, `service` | `image`, `build`                        | Compose が image/build を持つ             |

`dockerComposeFile` と `service` は片方だけ指定してはならない。`runServices` は Compose モード専用であり、指定する場合は `dockerComposeFile` と `service` も必須である。

### 対応プロパティ

| property                      | image   | Dockerfile | Compose | 備考                                                                                                                                                         |
| ----------------------------- | ------- | ---------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `image`                       | yes     | no         | no      | image-based mode                                                                                                                                             |
| `build.dockerfile`            | no      | yes        | no      | Dockerfile-based モード                                                                                                                                      |
| `build.context`               | no      | yes        | no      | `devcontainer.json` からの相対 path                                                                                                                          |
| `build.args`                  | no      | yes        | no      | string value のみ                                                                                                                                            |
| `build.options`               | no      | partial    | no      | Docker build argv に渡す。decune が管理する option と context path は不可                                                                                    |
| `build.target`                | no      | yes        | no      | multi-stage build target                                                                                                                                     |
| `build.cacheFrom`             | no      | partial    | no      | Docker CLI で扱える形式                                                                                                                                      |
| `dockerComposeFile`           | no      | no         | yes     | string / string array。local path のみ                                                                                                                       |
| `service`                     | no      | no         | yes     | primary service                                                                                                                                              |
| `runServices`                 | no      | no         | yes     | 未指定時は全 service。primary service は常に含める                                                                                                           |
| `features`                    | yes     | yes        | yes     | Compose モードは primary service final image に適用                                                                                                          |
| `overrideFeatureInstallOrder` | yes     | yes        | yes     | Feature install order に反映                                                                                                                                 |
| `overrideCommand`             | yes     | yes        | yes     | image/Dockerfile 既定 true、Compose 既定 false                                                                                                               |
| `mounts`                      | partial | partial    | partial | bind/volume 対応。Compose モードは primary service に override として追加。tmpfs は error                                                                    |
| `workspaceMount`              | yes     | yes        | no      | Compose モードは unsupported error。Compose file の primary service `volumes` を使う                                                                         |
| `workspaceFolder`             | yes     | yes        | yes     | Compose モードの既定は `/`                                                                                                                                   |
| `containerEnv`                | yes     | yes        | yes     | Compose モードは primary service `environment` override。secret storage ではない                                                                             |
| `remoteEnv`                   | yes     | yes        | yes     | exec/lifecycle/shell に適用。`${localEnv:...}` 由来 value は argv/log redaction 対象                                                                         |
| `remoteUser`                  | yes     | yes        | yes     | shell/lifecycle user                                                                                                                                         |
| `containerUser`               | yes     | yes        | yes     | Compose モードは primary service `user` override                                                                                                             |
| `updateRemoteUserUID`         | yes     | yes        | yes     | Linux host で既定 true                                                                                                                                       |
| `userEnvProbe`                | yes     | yes        | yes     | `none`, `loginShell`, `interactiveShell`, `loginInteractiveShell`                                                                                            |
| `forwardPorts`                | yes     | yes        | yes     | TCP-only。protocol suffix なしは TCP、`/tcp` は許可、`/udp` は unsupported error。Compose モードは `"service:port"` を受け付ける                             |
| `portsAttributes`             | partial | partial    | partial | `label`, `onAutoForward`, `requireLocalPort`。`protocol`, `elevateIfNeeded` は warning して無視                                                              |
| `otherPortsAttributes`        | partial | partial    | partial | automatic forwarding の既定。unsupported fields は warning                                                                                                   |
| `appPort`                     | yes     | yes        | no      | TCP-only。protocol suffix なしは TCP、`/tcp` は許可、`/udp` は unsupported error。Compose モードは unsupported error。Compose file の service `ports` を使う |
| `runArgs`                     | partial | partial    | no      | Compose モードは unsupported error。Compose file の service attributes を使う                                                                                |
| `init`                        | yes     | yes        | yes     | Compose モードは primary service `init` override                                                                                                             |
| `privileged`                  | yes     | yes        | yes     | Compose モードは primary service `privileged` override                                                                                                       |
| `capAdd`                      | yes     | yes        | yes     | Compose モードは primary service `cap_add` override                                                                                                          |
| `securityOpt`                 | yes     | yes        | yes     | Compose モードは primary service `security_opt` override                                                                                                     |
| lifecycle commands            | yes     | yes        | yes     | Feature metadata 由来 command は user command より前に実行                                                                                                   |
| `waitFor`                     | partial | partial    | partial | parse するが attached `up` は `postAttachCommand` まで同期実行                                                                                               |
| `name`                        | ignored | ignored    | ignored | runtime behavior には使わない                                                                                                                                |
| `shutdownAction`              | partial | partial    | partial | attached `up` 終了時に適用。明示 `down` / `remove` が正                                                                                                      |
| `hostRequirements`            | ignored | ignored    | ignored | warning                                                                                                                                                      |
| `customizations`              | ignored | ignored    | ignored | preserve するが実行しない                                                                                                                                    |

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

| Dev Container property | Compose モードの扱い | 代替                                                                                                                                                |
| ---------------------- | -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `workspaceMount`       | unsupported error    | workspace bind mount を primary service の `volumes` に書く                                                                                         |
| `appPort`              | unsupported error    | Docker published port 設定を Compose service の `ports` に書く                                                                                      |
| `runArgs`              | unsupported error    | `init`、`privileged`、`cap_add`、`security_opt`、`extra_hosts`、`dns`、`dns_search`、`devices`、`network_mode` など Compose service の field に書く |

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

### Clone isolation

Compose-based configuration の複数 clone を同一 Docker daemon 上で同時利用するとき、decune は次の境界で resource を分離する。

| Category                | Resources                                                                    |
| ----------------------- | ---------------------------------------------------------------------------- |
| Always workspace-scoped | project name、generated image、default network / volume                      |
| Opt-in rewrite          | fixed TCP port / name / IPv4 subnet、declared endpoint                       |
| No automatic rewrite    | external resource、IPv6、static service address、undeclared endpoint         |

常に workspace scope となる resource は、`safe_workspace_slug` と `workspace_id`、または workspace 固有の Compose project name により clone ごとに分離する。

Opt-in rewrite は `[compose.clone_isolation].enabled = true` を master gate とし、published port と固定名を workspace 固有値へ、network relocation と endpoint 契約を明示した対象を relocation 後の値へ書き換える。

自動 rewrite しない resource のうち、external resource は利用者の共有契約を維持する。relocation 対象 network の IPv6 / static address は actionable diagnostic で停止する。relocation 後も environment に残る旧 endpoint address は起動前に診断するが、宣言なしに値を推測して書き換えない。

clone isolation は external resource の clone 別複製や共有設定を自動化しない。また Compose YAML の merge、profiles、environment interpolation、relative path、build、network、volume semantics を再実装せず、Docker Compose v2 CLI の canonical model と実行結果を利用する。

#### Clone isolation preflight

Compose モードの `up` / `rebuild` は、user Compose file だけから得た canonical Compose model を使い、`docker compose up -d` の前に clone isolation preflight を常時実行する。`runServices` が指定されている場合、走査対象は primary service と `runServices`、Docker Compose がそれらの依存関係として展開した service、およびその service 群が使用する top-level resource に限定し、起動対象ではない service と未使用 resource は走査しない。`runServices` が指定されていない場合は Compose project 全体を走査する。preflight 自体は user Compose file を変更しない。`[compose.clone_isolation]` の name rewrite、network relocation、endpoint rewrite が有効な対象は generated override で書き換え、衝突照合にも書き換え後の値を使う。opt-in が無い対象は検出のみを行う。

対象:

- `networks.*.ipam.config[].subnet` に固定 IPv4 subnet を持つ non-external network。既存 Docker network の `IPAM.Config[].Subnet` と重複する場合、`compose_network_subnet_overlap` diagnostic で error にする。IPv6 subnet はこの preflight の重複判定対象外である。
- service の `container_name`。
- top-level `networks` / `volumes` / `configs` / `secrets` の `name:`。ただし Docker Compose が自 project name で scope した既定名と一致するものは固定名扱いしない。

`external: true` の top-level resource は、利用者が共有 resource として扱う契約なので clone isolation preflight の対象外である。

既存 Docker resource との照合では、`com.docker.compose.project` label が現在の decune Compose project name と一致する resource を自 project とみなし、衝突相手から除外する。label が無い resource は他 resource として扱い、衝突相手に含める。

固定 IPv4 subnet の重複は、同じ IPAM driver かつ同じ IPAM address space に属する network 間だけを衝突として扱う。IPAM driver 未指定は `default` とみなす。Compose network の既定 driver、`bridge`、`macvlan`、`ipvlan` は local address space、`overlay` は global address space とみなし、既存 Docker network の `Scope` が `local` なら local、`swarm` または `global` なら global とみなす。custom network driver、欠落した `Scope`、未知の `Scope` など address space を確定できない metadata は、実際の衝突を見逃さないため保守的に比較対象へ含める。

固定名が同種の既存 Docker resource と衝突する場合、`compose_fixed_name_conflict` diagnostic で error にする。診断 message には Compose 側の resource、要求した subnet/name、衝突相手の Docker resource name、衝突相手の `com.docker.compose.project` label があればその値を含める。

複数の衝突を検出した場合、preflight は最初の 1 件だけでなく、検出したすべての diagnostic を 1 回の error にまとめて報告する。

canonical Compose model に上記の clone-sensitive 構成が 1 つも無い場合、decune は clone isolation preflight のための Docker daemon resource 照会を行わない。

### generated Compose override

Compose モードで decune 固有機能を適用するため、state/runtime directory に generated override file を作る。この file は user が編集しない。

目的:

- primary service に decune label を付与する。
- primary service image を Feature/UID/GID/entrypoint 適用済み final image に差し替える。
- primary service image を decune generated local image に差し替える場合、元 Compose service の `pull_policy` を引き継いで registry pull しないよう、generated override で `pull_policy: never` を明示する。
- `containerEnv`、`containerUser`、`init`、`privileged`、`capAdd`、`securityOpt`、`mounts`、dotfiles mount、credential/runtime mount を primary service に追加する。
- `overrideCommand = true` の場合、primary service command を keepalive command に差し替える。
- Compose published port mapping/relocation で planned endpoint が requested endpoint と異なる service の `ports` を、planned `published` / `host_ip` を持つ list に置換する。
- clone isolation の name rewrite が有効な場合、対象 service の `container_name` と top-level resource の `name` を workspace 固有名へ書き換え、元の container name を network alias として追加し、対象 container name への service 内参照を追随させる。
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
- `container.cli.enabled`: boolean scalar として後勝ち。global の `false` は project の `true` で再有効化できる。
- `init` / `privileged`: boolean scalar として後勝ち。上位 layer の `false` は下位 layer の `true` を打ち消せる。
- `capAdd` / `securityOpt`: security list として deduped union。
- map: key ごとに merge。同一 key は後勝ち。
- decune TOML の array: 原則 append。ただし identity を持つ要素は置換。
- feature identity: canonical Feature ID と concrete ref。同一 concrete ref は option を merge する。`enabled = false` は canonical Feature ID 単位で無効化する。
- mount identity: `target`。
- dotfile identity: `target`。
- port identity: `protocol + service + container + host_ip`。service 未指定は primary service を表す。
- Compose published port mapping identity: `service + protocol + target`。同一 identity は後の layer が置換し、`enabled = false` は下位 layer の mapping を削除する。
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

[compose.published_ports]
automatic_relocation = false
warn_on_relocation = false

[[compose.published_ports.mappings]]
service = "app"
target = 502
protocol = "tcp"
host = 1502
host_ip = "127.0.0.1"

[container.cli]
enabled = true

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

dotfiles は host path を remote home に直接 bind mount しない。`/opt/decune/dotfiles/<target>` に mount し、container setup 時に remote user の home へ symlink を作る。`/opt/decune/dotfiles` と `/opt/decune/dotfile-backings` は decune の dotfiles 用 internal path として予約する。

- `source`: host path。global config では `~` または absolute path。project config の relative path は workspace root 相対。
- `target`: remote home からの相対 path。absolute path は禁止。
- `enabled`: 既定 true。false の場合は同一 target を無効化。
- `read_only`: 既定 true。
- `resolve_symlink`: 既定 true。true の場合は source を canonicalize する。file の場合は canonicalized source を直接 bind mount する。
- `on_conflict`: `fail`, `replace-symlink`, `backup`。既定 `fail`。

`resolve_symlink = true` の directory source では、配下に symlink がない場合は canonicalized source を直接 bind mount する。配下に symlink があり、同一 backing root に完全一致する場合は、その backing root を直接 bind mount する。

完全一致しない場合は state dir に mount 用 skeleton を作成する。skeleton root は `/opt/decune/dotfiles/<target>` に bind mount する。source 由来 file は個別 file bind mount ではなく、canonical parent directory を `/opt/decune/dotfile-backings/<n>` に bind mount し、skeleton 内に backing file への symlink を作る。`<n>` は dotfiles mount plan 全体で一意に採番する。同じ canonical parent directory と `read_only` を使う複数 entry は backing mount を共有し、`read_only` が異なる場合は別 target を割り当てる。symlink を含まない実ディレクトリは direct directory bind mount として表現する。skeleton、backing directory mount、direct directory mount の writable/read-only は `read_only` に従う。

backing parent directory 単位で mount するため、同じ parent directory の sibling file は `/opt/decune/dotfile-backings/<n>` 経由で container から見える。`read_only = false` の skeleton-only path に container から新規作成された file/directory は、元 source ではなく state dir の skeleton に保存し、以後の skeleton 準備でも保持する。ただし decune が計画した skeleton 内 symlink が container 内で regular file などに置換された場合、次回 skeleton 準備で計画どおりの symlink に戻す。`read_only = true` の skeleton では現在の dotfile tree に不要な stale entry を削除するが、既存 container の running reuse では skeleton を再生成しない。dotfile 内容は state dir にコピーしない。

通常ファイルの host 側 atomic replacement と、解決済み symlink target file の host 側 atomic replacement は、起動中 container から見える。source 側 symlink path 自体が host 側 rename で regular file に置換される場合は、起動中 container へ自動反映しない。反映には container recreate が必要である。

broken symlink、循環 symlink、特殊ファイル、mount 数の上限超過など、対応する bind mount plan として表現できない場合は error。

Compose モードでは primary service に dotfiles bind mount と setup lifecycle を適用する。

### `[[mounts]]`

任意の追加 mount。

- `type`: `bind`, `volume`, `tmpfs`。`bind` と `volume` に対応し、`tmpfs` は error。
- `source`: `bind` では必須。`volume` では volume 名。
- `target`: container absolute path。`/opt/decune` と `/run/decune` 配下、および workspace mount target と同一 target は禁止。特に `/opt/decune/dotfiles` と `/opt/decune/dotfile-backings` は dotfiles 用 internal path として予約する。
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

### `[compose.clone_isolation]`

同じ Compose-based workspace を複数の clone から同時起動するための opt-in 設定。`enabled` は master gate で、既定は false。false の場合、下位 table や `endpoints` が指定されていても無効として扱い、その内容は検証しない。ただし `endpoints` が 1 個以上あれば、無効な宣言を無言で無視せず warning を表示する。

```toml
[compose.clone_isolation]
enabled = false

[compose.clone_isolation.networks]
relocation = false
subnet_pool = "10.200.0.0/16"
# subnet_prefix = 24

[compose.clone_isolation.names]
rewrite_container_names = true
rewrite_resource_names = true

[[compose.clone_isolation.endpoints]]
service = "app"
env = "HOST_AGENT_ENDPOINT"
value = "grpc://${decune.network.fixed_net.gateway}:50051"
```

- `enabled`: 既定 false。false の場合、clone isolation による書き換えをすべて無効にする。true で `[compose.published_ports].automatic_relocation` が未指定の場合、その既定値を true に切り替える。global / project / CLI のいずれかで `automatic_relocation = false` が明示されていれば、明示値を優先する。
- `networks.relocation`: 既定 false。true の場合、固定 subnet を workspace ごとに relocation する対象とする。
- `networks.subnet_pool`: `networks.relocation = true` のとき必須。relocation 先を割り当てる IPv4 CIDR pool。`enabled = true` のとき、指定値が IPv4 CIDR でなければ error。
- `networks.subnet_prefix`: 任意。省略時は元 subnet の prefix 長を維持する。指定する場合は `subnet_pool` の prefix 以上かつ 31 未満でなければならない。
- `names.rewrite_container_names`: 既定 true。明示的な service `container_name` を workspace 固有名へ書き換える対象とする。
- `names.rewrite_resource_names`: 既定 true。top-level `name` を持つ `networks` / `volumes` / `configs` / `secrets` を workspace 固有名へ書き換える対象とする。
- `endpoints`: 0 個以上。`service` は環境変数を設定する Compose service、`env` は環境変数名、`value` は値 template。同一 `service` + `env` の重複宣言は error。

`endpoints.value` では `${decune.network.<compose-network-key>.gateway}` と `${decune.network.<compose-network-key>.subnet}` の 2 形式を clone isolation 専用 placeholder として予約する。これは一般の decune config 変数展開とは別に扱う。endpoint rewrite preflight は service と Compose network key の存在、および参照先 network が固定 IPv4 subnet relocation の対象であることを検証し、placeholder を planned gateway または CIDR 表記の planned subnet へ文字列置換する。未知または未終端の decune placeholder、存在しない service / network、relocation 対象でない network への参照は `compose_clone_isolation_invalid` で error にする。`enabled = true` でも `networks.relocation = false` のまま placeholder を参照した場合は、network relocation を有効にする設定 hint を付けて同じ diagnostic で error にする。decune placeholder 以外の `$` は literal として container environment へ渡し、Compose の host environment interpolation は適用しない。

render 後の値は generated override の `services.<service>.environment.<env>` に map 形式で書き込み、Compose の後勝ち map merge により user Compose file の値を置き換える。`!override` tag は使わない。元 IPAM config に gateway がなく `.gateway` placeholder が参照された場合に限り、planned subnet の先頭 host address を明示 gateway として network IPAM override に追加し、その値を render する。

`enabled = true` かつ `networks.relocation = true` の固定 IPv4 subnet relocation は次の契約に従う。

- slot 数は `2^(subnet_prefix - pool_prefix)` とする。`subnet_prefix` 省略時は元 subnet の prefix 長を使う。
- 初期 slot は SHA-256 の入力 `decune-clone-isolation-subnet-v1:<workspace_id>:<compose-network-key>` の先頭 8 byte を big-endian 整数として読み、slot 数で剰余を取って決める。そこから線形探索し、自 project 以外の同じ IPAM address space にある daemon network subnet、または同一 plan で割当済みの subnet と重複する slot を飛ばす。空きがなければ `compose_clone_isolation_pool_exhausted` で error にする。
- 別 process の relocation preflight と network 作成は atomic ではない。同じ初期 slot を選ぶ複数の `decune up` を同時に実行すると、相互の network が daemon snapshot にまだ現れず、後続の Docker network 作成が subnet 重複で失敗する場合がある。その場合は、先に成功した起動の network 作成後に失敗した `decune up` を再実行し、最新の daemon snapshot から再計画する。
- 元 IPAM config に gateway がある場合、元 subnet 内の host offset を新 subnet でも保存する。offset が新 prefix に収まらなければ `compose_clone_isolation_invalid` で error にする。元 gateway がなく、対応する endpoint 宣言から `.gateway` が参照されている場合は planned subnet の先頭 host address を明示 gateway として生成する。それ以外では gateway を生成しない。
- 元 IPAM config に `ip_range` がある場合は CIDR prefix と元 subnet 内の network address offset を、`aux_addresses` がある場合は各 map key と address offset を新 subnet でも保存する。field が IPv4 でない、元 subnet 外にある、または offset を新 prefix に収容できない場合は、Docker resource を変更する前に `compose_clone_isolation_unsupported` または `compose_clone_isolation_invalid` で停止する。diagnostic には network key と field 名を含め、field value 全体は含めない。
- 同じ Compose project の既存 network が、対象 Compose network key に対して pool 内の非重複 subnet を保持していれば最優先で再利用する。次に state の前回割当を再利用する。通常の `up` では blocker が消えても割当を維持し、requested subnet を再度優先するのは rebuild 時だけとする。
- 自 project の既存 network と新 plan の subnet、元 config の明示 gateway または endpoint 参照のために生成した gateway、`ip_range`、`aux_addresses` が一致しない場合、接続 container がなければ network を削除し、Compose に再作成させる。接続 container がある場合は `compose_clone_isolation_invalid` で停止し、`decune down` で project を停止してから `decune rebuild` するよう案内する。これには、旧 decune が `ip_range` / `aux_addresses` を欠落させて作成した network も含む。
- generated Compose override は、planned subnet が requested subnet と同じ場合も含め、top-level `networks.<key>.ipam.config: !override` で IPAM config list を置換し、`subnet`、明示 `gateway`、`ip_range`、`aux_addresses` を意味保存して再生成する。network の `driver` や IPAM の `driver` / `options` など config list 外の user 設定は変更しない。relocation が有効で固定 IPv4 subnet を検出した場合は、Compose `!override` tag のため Docker Compose v2.24.4 以上が必要で、version 判定不能または古い Compose は error にする。canonical Compose model の IPAM config に decune が意味を解釈できない field、または `subnet` のない config entry がある場合は、list の一部を黙って破棄せず `compose_clone_isolation_unsupported` で停止する。同じ network に未知 field が複数ある場合は、field 名を決定的順序ですべて列挙し、field value は含めない。
- `external: true` network は検出・書き換えの対象外とする。固定 IPv6 subnet、および対象 network に接続する service の `ipv4_address` / `ipv6_address` / `link_local_ips` は remap せず、`compose_clone_isolation_unsupported` で error にする。

network が実際に別 subnet へ relocate された場合、preflight はその network に直接接続する service と、`network_mode: service:<service>` で接続を継承する service を対象にする。canonical Compose model の `services.*.environment` に endpoint render 結果を後勝ちで重ねた実効 string value を走査し、元 subnet の基底 IPv4 address、または元 gateway が前後を数字・dot としない token 境界付きで残っていれば、`compose_clone_isolation_endpoint_unsafe` で `docker compose up` 前に error にする。endpoint 宣言があっても、同じ値に別の relocated network の旧 address が残っていれば error になる。`10.99.0.1` は `10.99.0.100` や `110.99.0.1` に一致しない。planned subnet が requested subnet と同じ場合は stale 検出を行わない。

stale 検出の対象は service environment value 内の元 subnet 基底 address と元 gateway だけである。`aux_addresses` 自体は IPAM config 内で remap するが、その元 address を environment、`extra_hosts`、service command、config file 内容などから参照していても自動検出・書き換えしないため、該当する外部 endpoint 契約は利用者が確認する。診断には service 名、環境変数名、Compose network key、一致した元 address だけを含め、environment value 全体を state、label、log、config hash、診断メッセージへ残してはならない。

`enabled = true` の name rewrite は generated Compose override に次の規則で出力する。

- `names.rewrite_container_names = true` のとき、service の明示的な `container_name: <name>` を `<name>-<workspace_id>` にする。`workspace_id` は canonical workspace path から算出する 12 桁 lowercase hex である。
- 書き換え対象 service が接続するすべての Compose network に元の `container_name` を network alias として追加する。user Compose file が service network を短縮 list 形式で指定していても、generated override の map 形式と Docker Compose の merge により alias を追加する。
- active な canonical Compose model 内で、書き換え対象 service の元 `container_name` を正確に参照する `services.*.network_mode` / `ipc` / `pid` の `container:<name>`、`volumes_from` の `container:<name>[:ro|rw]`、`external_links` の `<name>[:alias]` は、参照先だけを `<name>-<workspace_id>` へ追随させる。access mode と link alias は維持する。service 名を参照する entry と、書き換え対象ではない外部 container への entry は変更しない。
- `volumes_from` / `external_links` は generated override の `!override` list で完全置換し、書き換え対象外の entry も元の順序と値を維持して再出力する。この list rewrite が実際に必要な場合だけ Docker Compose v2.24.4 以上を要求し、version 判定不能または古い Compose は Docker resource を変更する前に `compose_clone_isolation_unsupported` で停止する。`network_mode` / `ipc` / `pid` の scalar rewrite だけならこの追加 version 条件を課さない。
- `names.rewrite_resource_names = true` のとき、top-level `networks` / `volumes` / `configs` / `secrets` の明示的な `name: <name>` を `<name>-<workspace_id>` にする。
- `external: true` の top-level resource は共有契約を維持し、書き換えない。

固定名 volume の書き換えは、clone ごとに別 volume を使いデータを分離する。元の `container_name` を指定して Compose project 外から実行する `docker exec <name>` などの tool は、書き換え後の名前へ追随する必要がある。Compose network 内から元名を使う接続は上記 alias で維持するが、namespace 共有、volume 継承、legacy link は DNS lookup ではないため、それぞれの明示参照を書き換える。

name rewrite の結果値である書き換え後の container/resource name、元 `container_name` のために生成する network alias、および追随して書き換える container name 参照は generated override semantic hash input に含めない。これらは workspace id と canonical Compose model から決定的に導出される relocation 結果値として扱う。name rewrite policy 自体と user Compose file の元名・元参照は従来どおり config hash input に含める。

### `[compose.published_ports]`

Docker Compose-based 構成の Compose service `ports` に対する automatic published port relocation policy と explicit mapping。

```toml
[compose.published_ports]
automatic_relocation = false
warn_on_relocation = false

[[compose.published_ports.mappings]]
service = "app"
target = 502
protocol = "tcp"
host = 1502
host_ip = "127.0.0.1"
```

- `automatic_relocation`: 既定 false。ただし `[compose.clone_isolation].enabled = true` かつ `automatic_relocation` 未指定の場合は既定 true。true の場合、対象となる fixed TCP published host port の requested endpoint が使えなければ、host 側 port number を変更する relocation candidate を自動探索してよい。
- `warn_on_relocation`: 既定 false。true の場合、後続の relocation 処理は requested endpoint と planned endpoint が異なる relocation について warning を出してよい。既存 Compose project の published binding を変更するために container 再作成を伴う場合の warning は、この設定に関係なく常に出す。
- `mappings`: fixed TCP published port の planned endpoint を明示する array。`automatic_relocation = false` でも有効であり、automatic relocation の有効/無効とは独立する。

`[[compose.published_ports.mappings]]` の field は次のとおり。

- `service`: 必須。Compose service 名。空文字列は error。
- `target`: 必須。Compose port entry の container 側 port。`1..=65535`。
- `protocol`: 任意。既定 `tcp`。`tcp` のみ対応する。
- `host`: enabled mapping では必須。planned host port。`1..=65535`。
- `host_ip`: 任意。IPv4 または IPv6 address。省略時は対応する Compose port entry の requested host IP を、host IP omitted を含めて継承する。
- `enabled`: 任意。既定 true。false の entry は `service + protocol + target` だけを identity として下位 layer の mapping を削除し、`host` / `host_ip` を指定してはならない。

同じ設定 file 内に同一 identity の mapping が複数ある場合は error とする。global/project 等の layer 間では通常の merge 順序に従って後の mapping が前の mapping を置換する。mapping の追加・変更・削除は config hash に含み、既存 project への反映には `decune rebuild` が必要になる場合がある。

CLI `--automatic-published-port-relocation` / `--no-automatic-published-port-relocation` は、この実行で `automatic_relocation` を override する。`--no-auto-forward` はこの policy を変更しない。

mapping と relocation の対象は fixed TCP published host port に限る。planning は以下の契約に従う。

- mapping は canonical Compose model の `service + protocol + target` に一致する port entry を解決する。存在しない service、active service 内で一致する entry が 0 件または複数件、または一致 entry が fixed TCP published host port でない場合は `compose_published_port_mapping_invalid` で起動前に error にする。存在するが今回の active service set に含まれない service の mapping はその実行では適用しない。
- 同じ port entry では explicit mapping、同一 Compose project の既存 binding、Compose file の requested endpoint の順に優先する。mapping の endpoint が reservation または availability probe と衝突した場合は `compose_published_port_mapping_conflict` とし、automatic relocation へ fallback しない。mapping 自身が requested endpoint と同じ場合も planning 対象だが、endpoint 差分がなければ generated override は不要である。
- 同一 Compose project で running 中の別 mapping identity が保持する endpoint は、rebuild planning でも reservation として扱う。複数 mapping の endpoint を相互に入れ替える場合、running project に対して atomic swap は行わず `compose_published_port_mapping_conflict` とする。既存 binding を解放するため `decune down` の後に `decune rebuild` を実行する。
- mapping により host port または host IP が変わる場合は relocation として扱う。既存 container の binding と異なれば再作成が必要であり、generated Compose override は `published` と `host_ip` の両方を planned endpoint に合わせる。

- image metadata や Feature metadata を merge した後の final `forwardPorts` / `[[ports]]` / CLI `-p` forwarding reservation を考慮し、同じ host endpoint を Compose published port と decune port forwarding の両方へ割り当ててはならない。
- mapping または automatic relocation が active な実行では、接続先 Docker daemon の running container を列挙し、`NetworkSettings.Ports` にある actual TCP published binding を外部 reservation として扱う。現在の Compose project label を持つ container はここから除外し、同 project 内の binding は既存 binding の規則で別途扱う。`docker ps` と inspect の間に container が消えた場合は残った inspect 結果で継続し、それ以外の list/inspect error は文脈付き hard error とする。
- 外部 reservation は requested endpoint と relocation candidate の両方に適用する。IPv4 wildcard `0.0.0.0` は IPv4 address と、IPv6 wildcard `::` は IPv6 address と同じ host port で衝突し、IPv4 と IPv6 の family は分離する。この判定は decune forwarding reservation と同じ helper contract を使う。
- availability probe は decune process からの TCP bind probe で行う。probe が `AddrInUse` で失敗した host port は occupied と扱う。probe が `PermissionDenied` で失敗した host port は、privileged port など decune process の権限では空き・占有を判別できない unprobeable な port として扱い、occupied とも available とも unexpected error とも区別する。`PermissionDenied` 以外の unexpected probe error は従来どおり hard error とする。
- 同一 Compose project の既存 container が同一 service / 同一 protocol / 同一 target port の published binding を持つ場合、requested port より既存 binding の host port 維持を優先する。running container 由来の binding は自 project が bind しているものとして availability probe なしで採用してよい。stopped container 由来の binding は実際には bind されていないため、採用前に availability probe を行う。stopped container 由来の binding が unprobeable な場合は、その binding を採用して実際の bind 成否を Docker daemon に委ねる。
- 既存 binding が使えない場合、requested host port を試す。requested host port が unprobeable な場合は、reservation と衝突していない限り requested host port を維持して実際の bind 成否を Docker daemon に委ねる。reservation には final forwarding reservation、同じ計画内で割り当て済みの Compose published port、同一 Compose project の running container 由来の既存 published binding を含める。stopped container 由来の binding は予約にはしない。requested host port が occupied または reservation と衝突する場合は、host IP の指定方法を維持したまま requested host port + 1 から昇順に relocation candidate を探索する。relocation candidate が unprobeable な場合は採用せず、次の candidate へ進む。OS assigned port fallback は行わず、65535 まで candidate がない場合は error にする。
- Docker actual binding reservation で検出できない process が unprobeable な requested host port または既存 binding を使用していた場合、Docker/Compose 起動時の published port collision diagnostic になる。
- 既存 container の actual published binding と新しい plan の planned endpoint が異なり、container 再作成しなければ起動できない場合、decune は published port relocation による再作成であることを warning し、`docker compose up --force-recreate` 相当で自動再作成して起動を継続する。この warning は `warn_on_relocation` と独立に常に出す。
- relocate 済み binding は、blocker が消えて requested host port が再び利用可能になっても維持する。requested host port へ戻すのは rebuild 時のみである。
- mapping または relocation により実際に host port か host IP を変更する場合、generated Compose override は Compose `!override` tag で service `ports` を置換する。このため Docker Compose v2.24.4 以上が必要で、version 判定不能または古い Compose では error にする。
- UDP、range、container-only port entry、`expose`、`network_mode: host` の service にある port mapping は relocation 対象外であり、存在するだけでは warning しない。

Compose published port diagnostics は automatic relocation policy の有効/無効とは別に、Docker Compose config から判定できる published port condition に対して使う。

- effective replica count が 2 以上の service が fixed TCP published host port を持つ場合、decune は replica ごとの published host port allocation を行わず `compose_published_port_multi_replica_unsupported` diagnostic で error にする。effective replica count は Docker Compose config の `scale`、なければ `deploy.replicas` から読む。
- invalid host IP、malformed port syntax、unexpected host port availability probe error は simple collision として扱わず、decune が判定できる場合は `compose_published_port_invalid` diagnostic で error にする。

Compose published port diagnostics の code は以下を使う。

- `compose_published_port_multi_replica_unsupported`: replica 数が 2 以上の service が、decune が対応しない fixed TCP published host port を持つ。
- `compose_published_port_unsupported`: startup failure が、host endpoint を安全に照合できる範囲で decune が対応しない Compose published port entry に関係している。
- `compose_published_port_invalid`: invalid host IP、malformed syntax、unexpected host port availability probe error など、simple collision ではない invalid published port condition。
- `compose_published_port_collision`: requested fixed TCP published host endpoint が unavailable。
- `compose_published_port_automatic_relocation_failed`: automatic relocation candidate を割り当てられない。
- `compose_published_port_bind_race`: planning 後に別 process が planned endpoint を取得した可能性がある。
- `compose_published_port_mapping_invalid`: mapping の service/identity が canonical Compose model の fixed TCP published port に一意に対応しない。
- `compose_published_port_mapping_conflict`: explicit mapping の desired endpoint が reservation または availability probe と衝突した。automatic relocation へは fallback しない。

### `[container.cli]`

```toml
[container.cli]
enabled = true
```

`enabled` は container 内の read-only decune CLI query を許可する project preference で、既定は true とする。global / project 間では通常の boolean scalar と同じ後勝ちで merge し、global の `false` は project の `true` で再有効化できる。`use_global_config = false` または `--no-global-config` では global 値を読み込まない。

effective value が false の場合は host daemon が query を拒否する authoritative gate とする。artifact の削除や symlink の有無だけを enforcement として扱わない。一方、この設定は untrusted repository から解除不能な security opt-out ではない。repository から解除できない deny policy が必要な場合は、credentials を含む host-only policy plane を別途設計する。

`container.cli.enabled` は config hash に含めない。この値だけの変更では container または Compose project の再作成を要求しない。

container query の collector は daemon 起動時に固定した server-side context だけを入力にする。state は固定 state directory の `state.toml` を query ごとに 1 回だけ読み、workspace path や config path を参照先として使わない。forwarding status も固定 status directory の全 session socket を query ごとに 1 回だけ集約する。host daemon は `ForwardStatusRegistry` を所有または注入されず、daemon owner と forwarding session owner が異なる場合も共有 status directory から全 session を検出する。1 session が停止した場合は、残る session だけを次の query に反映する。`Workspace::resolve`、config discovery、read-only up plan、build context hash は呼び出さない。

Docker container evidence は、固定 workspace ID の `decune.managed=true` resource と、固定 state または同 resource から導出した同一 Compose project だけを list / inspect / deduplicate する。Compose project label の候補は、固定 state に記録された値、または `decune.managed=true` かつ `decune.workspace_id` が固定 workspace ID と一致する resource の値に限定する。label value は trim 後に非空であることだけを確認し、Compose project name の形式検証は行わない。ここで検証するのは label の文字列形式ではなく、固定 query context または同一 workspace に帰属する managed resource から得た値であることを指す。request の command、format、path、resource name を Docker filter や host path に使わない。raw inspect、raw label map、stdout / stderr は container query の allowlist 型へ直ちに射影し、cache へ保存しない。status と ports は container / service / run state / health / config identity / published port を含む同じ container evidence snapshot を共有する。managed volume evidence は別 entry として取得する。

Docker evidence cache の key は server 側だけで次の値から作る。

```text
QueryEvidenceKey {
    query_context_fingerprint,
    workspace_id,
    kind: Containers | Volumes,
}
```

client input、workspace path、Docker resource name、output format は key に含めない。`Containers` は workspace container と同一 workspace の Compose project container の semantic load 全体、`Volumes` は managed volume evidence を表す。state と forwarding status は cache しない。

cache と query 専用 Docker 実行の内部固定値:

| 項目                            | 値     |
| ------------------------------- | ------ |
| concurrent Docker evidence load | 2      |
| Docker evidence load timeout    | 10 s   |
| query Docker command timeout    | 5 s    |
| success cache TTL               | 2 s    |
| failure cache TTL               | 500 ms |

TTL は load 完了時刻から数える。同一 key の cold load は semantic load 全体を singleflight し、waiter は同じ typed success または sanitized typed failure を共有する。異なる key を含め、実行中の Docker evidence load は全体で 2 件までとする。cache hit の Docker evidence は load 完了時点から最大 2 s stale になり得る。expired success の refresh が失敗した場合に stale result は返さない。Docker event 監視や mutation hook による invalidation は行わず、daemon 再生成時に cache を破棄する。

Docker evidence load は query coordinator が独立 task として所有する。呼出元の cancel だけでは load を中断せず、完了・failure・10 s timeout の全経路で waiter を wake する。query 専用 Docker command には既存 `RuntimeCommand` の timeout / kill / reap を使って 5 s timeout を設定し、通常の host `status` / `ports` / `up` の command timeout は変更しない。Docker failure は raw stderr を保持しない typed failure へ変換した後、collector の縮退規則に従って warning 付き snapshot へ変換する。query 全体の 15 s deadline と daemon admission は daemon dispatch が所有する。

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

通常の `up` / `rebuild` における host bind source の処理順:

1. `~` を展開。
2. `${...}` を展開。
3. relative path を基準 directory から absolute path にする。
4. `create = "directory"` なら directory を作成。
5. `resolve_symlink = true` なら canonicalize。
6. 存在しない path は `create` が指定されていない限り error。

`status <WORKSPACE>` の current config hash 計算では read-only のため、`create = "directory"` / `bind-create-src` で指定された missing path は作成しない。`resolve_symlink = true` の場合は既存 ancestor を canonicalize し、missing tail を合成した path を resolved mount として扱う。`resolve_symlink = false` の場合は既存 ancestor の存在を確認した上で、元の absolute path を resolved mount として扱う。`create` がない missing source は通常通り error とする。

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

Docker/Compose label から読み取る `decune.workspace_id` は、12 桁の lowercase hex (`[0-9a-f]{12}`) に完全一致する場合だけ workspace identity や state/runtime path の解決に使う。

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

config hash には、resolved metadata/config、Feature lock、relevant CLI options、Dockerfile 内容、`build.options`、effective ignore file、build context digest、entrypoint plan、Linux host の UID/GID sync input、Compose モードの user Compose files から得た sanitized canonical Compose model、Compose file digest、generated override semantic hash input を含める。manual/automatic forwarding の現在値、`container.cli.enabled`、Compose published port relocation により生成される service `ports` override、clone isolation network relocation により生成される subnet / gateway、credential token value、SSH agent socket path、GitHub token file path、`${localEnv:...}` 由来の `remoteEnv` value、Compose secrets の解決済み value は含めない。`${localEnv:...}` 由来の `containerEnv` value は平文では含めず、container 作成時環境の変更を検出するため非可逆 digest として含める。Compose モードでは user Compose files だけを対象にした `docker compose config --format json` が解決した interpolation / env file / profile / merge 結果から、`services.<service>.environment` の leaf value を平文ではなく digest marker に置き換えた canonical Compose model を hash に含める。この digest input は `decune-compose-env-value-hash-v1` で domain-separated / versioned にし、JSON path、JSON value type、canonical JSON value を含める。digest marker は `decune-compose-env-value-hash-v1:sha256:<hex>` 形式とし、environment value の平文を state、label、log、config hash input に残してはならない。generated override semantic hash input には primary service、decune が追加する label / environment / mount / user / security option / startup command、および decune generated image へ差し替えるかどうかを含める。`${localEnv:...}` 由来の `containerEnv` value は redacted marker または placeholder として扱い、実値を content hash 入力にしない。ただし generated override 内の `decune.config_hash` label や hash 由来 image tag など、hash 自身から派生する値は循環を避けるため hash 入力にしない。

clone isolation name rewrite により生成される container/resource name、元 `container_name` のために生成する network alias、追随して書き換える container name 参照、network relocation により生成される subnet / gateway、endpoint placeholder の render 後 environment value は relocation 結果値なので、generated override semantic hash input には含めない。clone isolation policy と endpoint の未展開 template は resolved config hash input に含める。

state file は `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` に保存する。write は atomic に行う。Docker/Compose label と state が矛盾する場合、container/project identity と config hash は runtime label を正とする。lifecycle 完了 marker と `devcontainer.json` path は state に記録し、creation lifecycle の二重実行や `up --config` 後の Compose project lifecycle 復元に使う。

state には起動時の mode を `image` / `dockerfile` / `compose` の snapshot として記録する。new container と reused container のどちらでも、その起動で解決した mode へ同期する。mode field がない既存の version 1 state は `unknown` として読み、state version は `1` を維持する。resolved config 全体や config 内容を mode snapshot のために保存しない。

Compose published port relocation では、requested endpoint、planned endpoint、`relocated`、起動時に Docker inspect で観測した actual binding を表示補助 metadata として state に記録する。この metadata は現在有効な Docker binding の正本ではない。

Compose clone isolation network relocation では、Compose network key ごとの requested subnet、planned subnet、planned gateway、`relocated` を表示補助 metadata として state に記録する。現在有効な subnet の正本は Docker network inspect とする。

`last_used_at` は `decune up` / `decune rebuild` が workspace を利用可能にした成功時だけ `unix:<seconds>` 形式で更新し、`created_at` / `last_started_at` から推測しない。`last_used_at` がない state の last-used 表示は unknown / `-` とする。`status`、`ports`、`down`、`remove` / `rm`、`clean` は state の last-used 情報を更新しない。

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

port forwarding と decune が生成する published port metadata は TCP-only とする。CLI `-p`、decune `[[ports]]`、Dev Container `forwardPorts`、Dev Container `appPort` は protocol suffix なしを TCP として扱い、`/tcp` は明示的な TCP 指定として受け付ける。`/udp` は unsupported error とする。decune は UDP forwarding と、Dev Container `appPort` からの UDP published port metadata 生成に対応しない。

`appPort` は image/Dockerfile モードの Docker published port であり container create 時に決まる。host IP が指定されない場合、Docker の既定で全 interface に公開される可能性があるため warning 対象とする。`appPort` の published port metadata も TCP-only である。

CLI `-p` と Dev Container `appPort` の host IP は IPv4 / hostname / bracketed IPv6 を受け付ける。IPv6 host IP は `[::1]:8080:3000` のように bracketed form で指定し、内部 model では bracket なしで保持する。unbracketed IPv6 は colon 区切りと曖昧なため error とする。`forwardPorts` string の `[::1]:3000` は host IP `::1` への forwarding として扱い、`[::1]:8080:3000` のような host-port mapping は `forwardPorts` では unsupported error とする。

Compose モードでは Docker published port 設定は Compose file の `ports` に委譲する。`appPort` は unsupported error とする。

`decune ports` は Docker container inspect から読み取った binding を表示するため、Docker published port に UDP binding が含まれる場合は、その binding も現在有効な host 側 port として表示する。

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

現在有効な host 側 port の利用状況は `decune ports` で確認できる。forwarding の実効対応は、`decune up` process が runtime directory に公開する host-local status socket に問い合わせる。Docker published port は Docker container inspect から binding を読み取る。forwarding の実効対応は `state.toml` には保存しない。Compose published port relocation の state metadata がある場合も、現在有効な published port は Docker container inspect から読み取った binding を正とする。stale metadata または接続不能な status socket は現在有効な forwarding ではないものとして無視する。

## Host daemon とセキュリティ境界

host daemon は `decune up` の子タスクとして起動し、`up` 終了時に停止する。常駐 system daemon ではない。

責務:

- Git credential helper request の処理。
- GitHub token file の一時管理。
- port forwarding runtime の socket 基盤。
- attached session の current workspace に限定した container CLI query の処理。

container-side tool と host daemon の JSON protocol version は `1` とする。request の `version` と `type` は top-level envelope で検証し、`credential` と `cliQuery` を request type として予約する。`cliQuery` は `version`、`type`、`command`、`format` だけを持つ strict schema とし、unknown field は拒否する。`status` + `text`、`ports` + `text`、`ports` + `json` だけを実行し、`status` + `json` とその他の未対応 format は `unsupported_format`、unknown command は `unsupported_command` とする。effective `container.cli.enabled` が false の場合、valid query は `container_cli_disabled` とする。reserved `portForward` request は `not_implemented` のまま維持する。

container query 用 model は、検証済み workspace ID、起動時 mode、container ID/name/service、run state/health、managed volume name、lifecycle/timestamp、sanitization 済み port だけを保持する。raw `ContainerInspect`、Docker/Compose label map、workspace/config path、raw config hash、env、build args、secret、mount source、external command の raw stderr、他 workspace の resource は model、cache、renderer へ渡さない。recorded state と runtime config identity の比較には raw hash を保持せず、domain-separated digest へ射影した非表示の比較値を使う。この比較値は `Debug` と serialization に出力しない。

container 専用 status は host の workspace detail model/renderer を流用せず、recorded state と query 時の managed runtime evidence を比較する。`Config snapshot: consistent` は両者が整合することだけを表し、live workspace config は常に `Live workspace: not checked` と表示する。recorded primary container が runtime evidence に存在しない場合、または identity を持つ managed container のいずれかが recorded identity と一致しない場合は `runtime-mismatch` とする。既知の identity 不一致がなく、primary container の identity を取得できない場合は `unavailable` とし、identity を持たない non-primary container は比較から除外する。state または runtime evidence 自体を取得できない場合も `unavailable` とし、host status の `current` / `needs-rebuild` とは区別する。health summary が `mixed` でも、実際に `unhealthy` な managed container がなければ `unhealthy-container` issue は表示しない。この issue の条件と severity (`error`) は host status と同じにする。host workspace/config path、raw hash/label は表示せず、host で実行する action は `Action (run on host)` section に表示する。text output の末尾 newline はちょうど 1 個とする。host 側 `decune status [WORKSPACE]` の live config 比較と既存 renderer は変更しない。

container query 用 port snapshot は workspace path と workspace ID field を構造上持たない。`ports` text は host の単一 workspace table と同じ column、意味、sort 順を使い、`ports` JSON は host の単一 workspace `Vec<PortInventoryEntry>` schema と同じにする。JSON の各 entry で `workspace` / `workspace_id` は `None` として省略し、text/JSON とも末尾 newline はちょうど 1 個とする。forwarding status socket の I/O と Docker evidence collector は host daemon 接続工程の責務とし、この snapshot/render 層には含めない。

container-side CLI の Cargo target は `decune-container-cli` とし、`clap` には依存せず `args_os` を使って解析する。container tools bundle では artifact name `decune` として配布し、runtime target と user-facing command は `/run/decune/decune` と `decune` とする。socket は既定で `/run/decune/host-daemon.sock` を使う。CLI の enabled/disabled lifecycle と container 内 symlink reconciliation は未接続である。

container-side CLI は current workspace だけを対象とし、`status` は `status` + `text`、`ports` は `ports` + `text`、`ports --json` は `ports` + `json` の query を送る。`status --json`、workspace positional（`.` を含む）、`ports --all`、重複する `ports --json` は socket へ接続する前に usage error とする。`up`、`rebuild`、`down`、`remove` / `rm`、`clean` も host-only command として local で拒否する。`--help` / `-h` / `help`、command help、`--version` / `-V` は local 表示とし、host-only command の help は host で実行する command であることを説明する。help option は argument を左から解析して到達した時点で local help を表示し、それより前に検出した unknown option や重複 option は usage error とする。引数なし、unknown command / option、non-UTF-8 argument は panic せず usage error とする。

query transport は request の write 完了後に Unix socket の write half を shutdown し、response を EOF まで読む。container-side CLI が受理する response は最大 1 MiB（1,048,576 bytes）とし、上限を超える response は invalid response として拒否する。daemon handoff 中の socket 交換を許容するため、connect の `NotFound` / `ConnectionRefused` だけを最大 5 回、100 ms 間隔で再試行する。permission error、request write/read error、invalid response、daemon error は再試行しない。再試行を使い切った場合は、attached `decune up` session が必要で detached mode では利用できないことを示す canonical unavailable error とする。他の transport error は daemon 停止や authorization failure と断定しない generic error とする。

host daemon response は `version`、`ok`、任意の `output`、任意の `error`、任意の `warnings` を持つ。success response は `output` が必須で `error` を持たず、warning を 0 件以上持てる。error response は `code` と `message` を持つ `error` が必須で、`output` と warning を持たない。client はこの invariant に違反する response を invalid response として拒否する。`warnings` がない version 1 response は空 warning list として扱う。

container-side CLI は success warning を配列順に `Warning: <message>` として stderr へ書き、success output は改変せず stdout へ書く。success は warning の有無にかかわらず exit `0`、daemon / transport / invalid-response error は exit `1`、usage error は exit `2` とする。daemon error code は未知の将来値も受理し、その message を `Error: <message>` として stderr へ書き、stdout は空に保つ。warning と error の末尾 newline は 1 個に正規化するが、success output には newline を追加しない。

host daemon error code は lowercase snake_case とし、`invalid_request`、`unsupported_protocol_version`、`request_too_large`、`unknown_request_type`、`not_implemented`、`credential_failed`、`unsupported_command`、`unsupported_format`、`container_cli_disabled`、`cli_query_failed`、`cli_query_busy`、`cli_query_timeout` を定義する。wire 上の `code` は将来の追加値を受理できる string とする。

host daemon の connection / query admission と I/O の固定上限は次のとおりとする。

| 項目                          | 上限 |
| ----------------------------- | ---: |
| active host daemon connection |   32 |
| request body read timeout     |  2 s |
| response write timeout        |  2 s |
| active `cliQuery`             |    8 |
| `cliQuery` total timeout      | 15 s |

connection permit は listener から accept して task を生成する前に確保する。上限中は新しい connection task を生成せず、接続を listener / OS backlog 側で待たせる。credential、reserved request、`cliQuery` を含む全 connection を同じ上限に数え、既存の peer UID 検証、protocol version 検証、request body の 64 KiB 上限を維持する。request body を 2 s 以内に EOF まで読めない場合は connection を閉じる。response の `write_all` と write half shutdown は合わせて 2 s 以内に完了させる。read / write timeout または I/O error では完全な wire response を保証せず、client は transport error として扱う。

`cliQuery` は envelope、strict schema、policy、command / format matrix をすべて検証した後にだけ query semaphore へ入れる。credential request は query semaphore を使わない。8 件の permit は待機せず `try_acquire` し、上限時は即座に `cli_query_busy` を返す。15 s deadline は permit 取得後から state、forwarding、Docker/cache 待機、render、success response 生成までを含み、socket write の 2 s timeout は含めない。deadline 超過は `cli_query_timeout`、その他の fatal collector / render failure は `cli_query_failed` とする。いずれの error response にも partial output と warning を含めない。

degradable な state、forwarding、Docker diagnostic は、prefix と末尾 newline を持たない sanitized message として success response の `warnings` に格納する。text / JSON の完成済み output は `output` だけに格納し、特に `ports` JSON へ warning を混在させない。success output / warning と error response には、secret、raw config hash / label、host path、他 workspace の情報、external command の raw stderr を含めない。container CLI query は active attached `decune up` session の host daemon が存在する間だけ利用でき、detached mode は対象外とする。

host daemon は effective `container.cli.enabled` と immutable query context を起動時に固定する。query context は検証済み workspace ID、その ID に対応する state directory、workspace runtime directory、そこから導出する forwarding status directory だけを保持し、`Workspace::resolve`、live config、client input から host path を再解決しない。context fingerprint は `decune-cli-query-context-v1` で domain separation した SHA-256 digest とし、canonical field order で workspace ID と固定 server path context だけを入力にする。secret、token、credential value、resolved config 全体を入力に含めない。

host daemon metadata には query policy と context fingerprint だけを保存し、raw context、host path、secret を保存しない。daemon reuse identity は `Disabled` または `Enabled { context_fingerprint }` とし、disabled 同士、または同じ fingerprint の enabled 同士だけを再利用できる。policy または context が異なる active daemon は暗黙に共有せず、対象 workspace のすべての active attached `decune up` を終了してから再実行するよう error にする。reused daemon を監視する session は owner 終了後も同じ policy と実体 query context で daemon を再起動する。protocol version、peer UID/GID、Git HTTPS mode、socket inode 等の既存 reuse 条件も維持する。

protocol version は `1` のままとし、capability list、build SHA、daemon revision は追加しない。decune v0 段階では旧/new daemon-client の mixed-version compatibility を保証しない。upgrade 時は対象 workspace のすべての active `decune up` を終了してから、新しい version で起動し直す。reuse 判定で active daemon の metadata を現在の version として読めない場合、または protocol version が一致しない場合も暗黙に共有せず、version 不一致の可能性を示してすべての active `decune up` の終了を促す error にする。

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
