# decune 内部設計ノート

この文書は、`decune` の内部実装の現状を貢献者向けに説明する参考情報です。公開挙動の正ではありません。挙動の契約は [specification.md](specification.md) にあり、この文書と実装が specification.md と矛盾する場合は specification.md と実装を正として、この文書を追随させます。

この文書に書く上限値、timeout、TTL などの数値は、公開契約ではなく執筆時点の実装定数です。変更しても仕様変更ではなく、この文書の更新として扱います。検証手順と執筆規約は [development.md](development.md) を参照してください。

## 1. Cargo workspace 構成

Cargo workspace は 4 つの crate で構成します。

- `decune`(リポジトリ直下): host 側 CLI 本体。単一 binary `decune` を提供する。
- `crates/decune-container-protocol`: host daemon と container-side tools が共有する wire protocol 型の library。
- `crates/decune-container-tools`: container 側の 3 binary。
- `xtask`: 開発・リリース用の automation task。

`decune-container-protocol` は、protocol version 定数(`HOST_DAEMON_PROTOCOL_VERSION = 1`)、request type(`credential` / `cliQuery`)、daemon error code の文字列定数、request / response envelope 型(`GitCredentialHostRequest`、`CliQueryRequest`、`HostDaemonResponse`、`HostDaemonError`)、forward agent の scan request / response 型を定義します。依存は `serde` だけで、host 側(`src/host/`)と container-side tools の両方が同じ型で wire format を読み書きします。

`decune-container-tools` の binary は `decune-container-cli`、`decune-forward-agent`、`git-credential-decune` の 3 つです。`clap` に依存せず、container 内 CLI は `args_os` を手続き的に解析します(`src/bin/decune_container_cli/parser.rs`)。bundle への格納時に `decune-container-cli` は artifact name `decune` へ改名します(`xtask/src/container_tools.rs` の対応表)。container 内 `/run/decune/decune` の実体はこの binary であり、host 側 `decune` binary ではありません。

crate 間の依存は次の形です。

- `decune` と `decune-container-tools` は `decune-container-protocol` に path 依存する。
- `decune` は `decune-container-tools` にコンパイル時依存しない。container tools は build script(`build.rs`)が bundle file として埋め込む(6 節)。
- `xtask` は workspace 内 crate に依存せず、`cargo` / `rustup` を子 process として呼ぶ。

host 側 `decune` binary は、argv[0] の file name が `git-credential-decune` または `decune-forward-agent` に一致する場合、その tool として動作する分岐を `src/main.rs` に持ちます。本体 crate 側にも同等の helper / agent 実装があり(`src/host/credentials/`、`src/host/forward/`)、host 側 test はこの実装を in-process 実行にも使います。container へ配置する artifact は `decune-container-tools` の独立 binary です。

`xtask` の subcommand は `build-container-tools` / `check-container-tools` / `install` / `dist` / `checksum` / `release-manifest` / `release-preflight` / `workspace-test` / `compose-integration` です。使い方は [development.md](development.md) にあります。

## 2. ランタイムアダプター構成

外部コマンドは `std::process::Command` / `tokio::process::Command` に argv 配列を渡して実行します。これは [specification.md 12.2 節](specification.md#122-外部コマンド実行と-redaction)の argv 実行原則の実装形です。

adapter は次の 3 つです。

- `DockerCli`: `docker` の存在確認、version、image/container/exec/cp/inspect/build/pull/rm/stop/start/wait/port 相当の操作。
- `DockerComposeCli`: `docker compose` の存在確認、`version --short`、required capability probe、config/build/up/stop/down/ps/logs/pull 相当の操作。
- `RuntimeCommand`: command 実行、stdout/stderr capture、streaming、exit status、timeout、signal handling、redaction の共通基盤。

JSON を読む操作は、次の CLI JSON 出力を serde 型へ parse します。

- `docker image inspect --format json` または `docker inspect --format json`
- `docker compose config --format json`
- `docker compose ps --format json`

argv 実行原則、error 変換、redaction の契約は specification.md 12.2 節、Podman 互換性は [2.4 節](specification.md#24-podman-互換性)にあります。

## 3. host daemon の実装

host daemon の connection / query admission と I/O の固定上限は次のとおりです(`src/host/daemon.rs`、`src/host/query.rs` の実装定数)。

| 項目                          | 現在値 |
| ----------------------------- | -----: |
| active host daemon connection |     32 |
| request body 上限             | 64 KiB |
| request body read timeout     |    2 s |
| response write timeout        |    2 s |
| active `cliQuery`             |      8 |
| `cliQuery` total timeout      |   15 s |

connection permit は listener から accept して task を生成する前に確保します。上限中は新しい connection task を生成せず、接続は listener / OS backlog 側で待たせます。credential、reserved request、`cliQuery` を含む全 connection を同じ上限で数え、peer UID 検証、protocol version 検証、request body の 64 KiB 上限を適用します。request body を 2 s 以内に EOF まで読めない場合は connection を閉じ、response の `write_all` と write half shutdown は合わせて 2 s 以内に完了させます。

host daemon の停止時は accept loop と accepted connection task を中断します。accept の失敗で accept loop だけが終了した場合は、新規 connection の受付を止め、処理中の connection task は中断せず完了まで待ちます。read / write timeout、I/O error、daemon 停止による task 中断では完全な wire response を保証せず、client は transport error として扱います([specification.md 3.9 節](specification.md#39-container-内の-decune-cli))。

`cliQuery` は envelope、strict schema、policy、command / format matrix をすべて検証した後にだけ query semaphore へ入れます。credential request は query semaphore を使いません。8 件の permit は待機せず `try_acquire` し、上限時は即座に `cli_query_busy` を返します。15 s deadline は permit 取得後から state、forwarding、Docker/cache 待機、render、response 構築、JSON serialization までを含み、socket write の 2 s timeout は含みません。deadline 超過は `cli_query_timeout`、その他の fatal collector / render / serialization failure は `cli_query_failed` になります。いずれの error response にも partial output と warning を含めません(error code の定義は [specification.md 13.3 節](specification.md#133-host-daemon-error-code))。

query context fingerprint は、`decune-cli-query-context-v1` で domain separation した SHA-256 digest です。canonical field order で workspace ID と固定 server path context だけを入力にし、secret、token、credential value、resolved config 全体を入力に含めません。host daemon metadata(runtime directory の `host-daemon.json`)には query policy と context fingerprint だけを保存し、raw context、host path、secret を保存しません。daemon reuse identity は `Disabled` または `Enabled { context_fingerprint }` の 2 値です。

## 4. container CLI query の collector と evidence cache

container query の collector は、daemon 起動時に固定した server-side context だけを入力にします。state は固定 state directory の `state.toml` を query ごとに 1 回だけ読み、workspace path や config path を参照先として使いません。forwarding status も固定 status directory の全 session socket を query ごとに 1 回だけ集約します(8.4 節)。host daemon は `ForwardStatusRegistry` を所有または注入されず、daemon owner と forwarding session owner が異なる場合も共有 status directory から全 session を検出します。1 session が停止した場合は、残る session だけが次の query に反映されます。`Workspace::resolve`、config discovery、read-only up plan、build context hash は呼び出しません。

Docker container evidence は、固定 workspace ID の `decune.managed=true` resource と、固定 state または同 resource から導出した同一 Compose project だけを list / inspect / deduplicate します。Compose project label の候補は、固定 state に記録された値、または `decune.managed=true` かつ `decune.workspace_id` が固定 workspace ID と一致する resource の値に限定します。label value は trim 後に非空であることだけを確認し、Compose project name の形式検証は行いません。ここで確認しているのは label の文字列形式ではなく、固定 query context または同一 workspace に帰属する managed resource から得た値であることです。request の command、format、path、resource name は Docker filter や host path に使いません。raw inspect、raw label map、stdout / stderr は container query の allowlist 型へ直ちに射影し、cache へ保存しません。status と ports は container / service / run state / health / config identity / published port を含む同じ container evidence snapshot を共有し、managed volume evidence は別 entry として取得します。

Docker evidence cache の key は server 側だけで次の値から作ります。

```text
QueryEvidenceKey {
    query_context_fingerprint,
    workspace_id,
    kind: Containers | Volumes,
}
```

client input、workspace path、Docker resource name、output format は key に含めません。`Containers` は workspace container と同一 workspace の Compose project container の semantic load 全体、`Volumes` は managed volume evidence を表します。state と forwarding status は cache しません。

cache と query 専用 Docker 実行の内部固定値は次のとおりです(`src/host/query.rs` の実装定数)。

| 項目                            | 現在値 |
| ------------------------------- | ------ |
| concurrent Docker evidence load | 2      |
| Docker evidence load timeout    | 10 s   |
| query Docker command timeout    | 5 s    |
| success cache TTL               | 2 s    |
| failure cache TTL               | 500 ms |

TTL は load 完了時刻から数えます。同一 key の cold load は semantic load 全体を singleflight し、waiter は同じ typed success または sanitized typed failure を共有します。異なる key を含め、実行中の Docker evidence load は全体で 2 件までです。cache hit の Docker evidence は load 完了時点から最大 2 s stale になり得ます(公開契約としての縮退記述は [specification.md 12.5 節](specification.md#125-container-cli-query-の境界))。expired success の refresh が失敗した場合に stale result は返しません。Docker event 監視や mutation hook による invalidation は行わず、daemon 再生成時に cache を破棄します。

Docker evidence load は query coordinator が独立 task として所有します。呼出元の cancel だけでは load を中断せず、完了・failure・10 s timeout の全経路で waiter を wake します。query 専用 Docker command には `RuntimeCommand` の timeout / kill / reap を使って 5 s timeout を設定し、通常の host `status` / `ports` / `up` の command timeout は変更しません。Docker failure は raw stderr を保持しない typed failure へ変換した後、collector の縮退規則に従って warning 付き snapshot へ変換します。query 全体の 15 s deadline と daemon admission は daemon dispatch が所有します(3 節)。

recorded state と runtime config identity の比較には raw hash を保持せず、domain-separated digest へ射影した非表示の比較値を使います。この比較値は `Debug` と serialization に出力しません。

## 5. container 内 CLI の transport 内部

container 内 CLI の transport は、daemon handoff 中の socket 交換を許容するため、connect の `NotFound` / `ConnectionRefused` だけを再試行します。接続試行は合計最大 5 回、試行間隔は 100 ms です(初回 1 回 + 再試行 4 回。`crates/decune-container-tools/src/bin/decune_container_cli/transport.rs`)。response 上限 1 MiB、再試行しない error 種別、canonical unavailable error は公開契約として [specification.md 3.9 節](specification.md#39-container-内の-decune-cli)にあります。

## 6. container tools bundle と runtime staging

container-side tools は release build 時に host binary へ埋め込みます。bundle は `git-credential-decune`、`decune-forward-agent`、`decune`(Cargo binary target は `decune-container-cli`)の 3 tools を各 platform に 1 artifact ずつ持ち、現在の 2 platform(`linux-amd64` = `x86_64-unknown-linux-musl`、`linux-arm64` = `aarch64-unknown-linux-musl`)で 6 artifact になります。bundle directory には、schema version と protocol version、artifact ごとの name / platform / path / SHA-256 を記録した `manifest.json` を置きます。

build 時の埋め込みは `build.rs` が行い、次の内部環境変数で制御します(7 節)。

- `DECUNE_CONTAINER_TOOLS_BUNDLE`: `auto`(既定。bundle directory に `manifest.json` があれば検証して埋め込み、なければ埋め込みなし)、`required`(bundle 必須。欠落・検証失敗は build error)、`off`(埋め込みなし)。
- `DECUNE_CONTAINER_TOOLS_BUNDLE_DIR`: bundle directory の場所。既定は `target/decune-xtask/container-tools-bundle`。

`cargo run --locked -p xtask -- install --locked` は bundle を build/check し、`DECUNE_CONTAINER_TOOLS_BUNDLE=required` を設定した `cargo install --profile dist` で bundle 埋め込みの `decune` を install します。通常の local / CI 手順ではこれらの環境変数を直接使わず、`xtask` が内部で設定します。

開発・debug 用の runtime override として `DECUNE_CONTAINER_TOOLS_DIR` があります。設定すると、埋め込み bundle の代わりに指定 directory の bundle(`manifest.json` 必須)を検証して使います。埋め込みなしの build で override も未設定の場合、container tools を必要とする操作は error になります。

container-side tool の runtime staging は、container に mount する runtime directory 内へ temporary file を作りません。host-private かつ target と同一 filesystem の親 directory に排他的 create で temporary file を作り、開いた file descriptor への artifact bytes の書き込み、mode `0755` の設定、最終 staged bytes の SHA-256 検証が完了した後、runtime target を atomic rename で置換します。既存 target が symlink の場合は link 先を変更せず symlink entry 自体を置換し、directory など安全に置換できない file type は runtime corruption error にします。失敗時は temporary file を削除し、partial target を公開しません(公開契約は [specification.md 11 章](specification.md#11-配布の契約))。

## 7. 内部環境変数

次の環境変数は decune 内部の契約で、公開契約ではありません。名前と意味は予告なく変わり得ます。

host 側 `decune` が読むもの:

- `DECUNE_DOCKER_RESOURCE_LOCK`: Docker resource 操作を process 間で直列化する flock file の path。未設定なら lock は無効。並行 test / CI 向けの内部 escape hatch(`src/docker/lock.rs`)。
- `DECUNE_CONTAINER_TOOLS_DIR`: 埋め込み bundle の代わりに使う外部 container tools bundle の directory(6 節)。

host が container 側 process へ渡すもの:

- `DECUNE_FORWARD_AGENT_SOCKET` / `DECUNE_FORWARD_AGENT_SECRET` / `DECUNE_FORWARD_AGENT_ALLOWED_PORTS`: forward agent を `docker exec` で起動するときの env。socket は agent が listen する container 内 Unix socket path、secret は host と agent の接続認証に使う random hex(redaction 登録済みで、agent は起動後に自 process の環境から除去する)、allowed ports は転送を許可する container port のカンマ区切りリスト。
- `DECUNE_REMOTE_USER` や `DECUNE_GH_CONFIG_OWNER` など、decune が生成する exec script 向けの補助変数。
- generated Compose override は `${DECUNE_CONTAINER_ENV_<KEY>}` 形式の placeholder を参照し、secret-sensitive な containerEnv 値を override file に直書きせず子 process の環境として渡す。

container-side tools が読むもの:

- `DECUNE_HOST_DAEMON_SOCKET`: `decune-container-cli` と `git-credential-decune` の接続先 socket の override。未設定時は既定の `/run/decune/host-daemon.sock` を使う。通常経路では設定されず、test / debug 用。

build 時に `build.rs` が読むもの:

- `DECUNE_CONTAINER_TOOLS_BUNDLE` / `DECUNE_CONTAINER_TOOLS_BUNDLE_DIR`(6 節)。

このほかに、build script が version 表示用に設定する rustc-env(`DECUNE_DISPLAY_VERSION` など)と、test 専用の変数(`DECUNE_TEST_*`、`DECUNE_E2E_*` など)がありますが、runtime の内部契約ではないためこの一覧には含めません。

## 8. host 側 directory と生成 file レイアウト

### 8.1 directory の解決

workspace id は、canonical 化した workspace root path の SHA-256 digest の先頭 12 hex 文字です(`src/workspace.rs`)。workspace 単位の directory は次の場所に作ります。

| directory             | 場所                                     | XDG 未設定時の fallback            |
| --------------------- | ---------------------------------------- | ---------------------------------- |
| state directory       | `$XDG_STATE_HOME/decune/<workspace_id>`  | `~/.local/state/decune/...`        |
| runtime directory     | `$XDG_RUNTIME_DIR/decune/<workspace_id>` | `/tmp/decune-<uid>/<workspace_id>` |
| cache directory       | `$XDG_CACHE_HOME/decune/<workspace_id>`  | `~/.cache/decune/...`              |
| Feature archive cache | `$XDG_CACHE_HOME/decune/features`        | `~/.cache/decune/features`         |

runtime directory の親(`decune` / `decune-<uid>`)と runtime directory 自体は 0700 に設定・検証します(`src/host/runtime.rs`)。

### 8.2 state directory

state directory 直下に生成する file:

- `state.toml`: workspace state(9 節)。
- `state.toml.tmp.<pid>.<nanos>`: atomic write 用の temporary file。rename 後は残らない。
- `compose.override.yaml`: Compose モードで decune が生成する Compose override。

`.decune/features.lock.toml` は state directory ではなく workspace 側の file で、Feature archive cache は cache 側の共有 directory です。

### 8.3 runtime directory

runtime directory は全体を container の `/run/decune` へ bind mount します。配下の file と作り手は次のとおりです。

| file                              | 作り手                       | 内容                                                                                     |
| --------------------------------- | ---------------------------- | ---------------------------------------------------------------------------------------- |
| `host-daemon.sock`                | host daemon                  | credential / `cliQuery` 用 Unix socket                                                   |
| `host-daemon.json`                | host daemon                  | daemon reuse metadata(query policy と context fingerprint。0600)                         |
| `decune`                          | host(staging)                | container CLI artifact(実体は `decune-container-cli`)                                    |
| `git-credential-decune`           | host(staging)                | Git credential helper artifact                                                           |
| `host-gitconfig`                  | host                         | host の `~/.gitconfig` の copy(`copy_global_config` 有効時。0600)                        |
| `decune-forward-agent`            | host(staging)                | forward agent artifact                                                                   |
| `forward-agent-<session_id>.sock` | container 内の forward agent | forwarding session の Unix socket(単一 session 名は `forward-agent.sock`)                |
| `forward-agent.err`               | container 内の forward agent | agent 失敗時の診断 message                                                               |
| `forward-agent.status`            | container 内の forward agent | agent の終了 status(host が起動失敗の検出に読む)                                         |
| `secrets/github-token`            | host                         | GitHub CLI token(directory 0700 / file 0600。container へは read-only file mount)        |
| `feature-entrypoints-complete`    | host                         | Feature entrypoint 完了を伝える sentinel                                                 |
| `feature-entrypoints-token`       | host                         | Feature entrypoint 用 token                                                              |
| `forward/<service_key>/`          | host                         | Compose sidecar forwarding 用の service 固有 runtime directory(forward agent だけを含む) |

container 側だけに現れる関連 mount として、`/run/decune/gh`(GitHub CLI 設定用の writable tmpfs。`GH_CONFIG_DIR` が指す)と `/run/decune/ssh-agent.sock`(host の `SSH_AUTH_SOCK` socket の bind mount)があります。これらの source は runtime directory 内の file ではありません。

### 8.4 forwarding status directory

forwarding status directory は runtime directory の兄弟 directory `<runtime_dir>-ports`(0700)です。forwarding session ごとに host 側の status server が次の 2 file を作ります。

- `forward-status-<session_id>.sock`: session の forwarding 一覧を返す Unix socket(0600)。
- `forward-status-<session_id>.json`: version、session id、socket name、pid を持つ metadata(0600)。

集約(`decune ports` の host 表示や container CLI query の forwarding 情報)は、status directory の metadata file を列挙・sort し、各 socket へ `list` request を送って全 session の forwarding を連結します。connect が `NotFound` / `ConnectionRefused` になる socket は stale として黙って skip し、その他の failure は warning にします。session の停止時は socket と metadata を削除し、次回の集約から自然に消えます。host daemon はこの仕組みだけで全 session を発見するため、forwarding session の owner process と daemon owner が異なっても集約できます(4 節)。

### 8.5 container 内のその他の internal path

- `/opt/decune/dotfiles` と `/opt/decune/dotfile-backings`: dotfiles の mount 先と backing mount。
- `/opt/decune/cache`: decune 管理の cache mount。
- `/usr/local/share/decune/feature-entrypoint-wrapper.sh`: Feature entrypoint wrapper。runtime directory ではなく image build 時に焼き込む。

`/run/decune` と `/opt/decune` が user-defined mount target として使えない制約は [specification.md](specification.md) にあります。

### 8.6 legacy path

GitHub token の legacy runtime path `gh-token/token`(container target `/run/decune/gh-token`)は、新規 staging には使いません。現行実装(`src/host/credentials/github.rs`、`src/up/existing.rs`)では cleanup 対象として削除し、legacy mount を持つ既存 container を stale とみなして recreate させる判定にだけ使います。公開契約の path は `/run/decune/secrets/github-token` だけです。

## 9. state.toml のキー構成

state file の公開挙動は [specification.md 10.4 節](specification.md#104-state-file)が定める互換性契約(version、unknown mode の読み、atomic write、label 優先)だけで、全キーの構成は内部形式です。serde 構造体(`src/state.rs`)は unknown field を拒否します。

トップレベルのキー:

| キー                   | 内容                                                                                                                                                                                    |
| ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `version`              | state version。現在 `1`                                                                                                                                                                 |
| `workspace`            | workspace root の表示用 path                                                                                                                                                            |
| `mode`                 | 起動時 mode snapshot(`image` / `dockerfile` / `compose`。field がない state は `unknown`)                                                                                               |
| `container_id`         | primary container の ID                                                                                                                                                                 |
| `image`                | 起動に使った image                                                                                                                                                                      |
| `config_hash`          | config hash(10 節)                                                                                                                                                                      |
| `config_file`          | `--config` で指定した `devcontainer.json` path(省略可)                                                                                                                                  |
| `compose_project_name` | Compose project name(Compose モードのみ)                                                                                                                                                |
| `created_at`           | 作成時刻(`unix:<seconds>` 形式)                                                                                                                                                         |
| `last_started_at`      | 最終起動時刻(同上)                                                                                                                                                                      |
| `last_used_at`         | 最終利用時刻(同上。ない state もある)                                                                                                                                                   |
| lifecycle 完了 flag    | `on_create_completed` / `after_on_create_completed` / `update_content_completed` / `after_update_content_completed` / `post_create_completed` / `after_post_create_completed` の 6 bool |

`[[published_ports]]`(Compose published port relocation の表示補助 metadata):

- `source`(現在 `compose` のみ)、`type`(現在 `published` のみ)、`service`、`port_entry_index`。
- `target`: `port` と `protocol`。
- `requested` / `planned`: `host_ip_kind`(`omitted` / `explicit`)、`host_ip_value`(explicit 時のみ)、`host_port`。
- `actual_bindings`: 起動時に Docker inspect で観測した `host_ip` / `host_port` の配列。
- `relocated`: relocation が起きたかどうか。

`[clone_isolation]`(network relocation の表示補助 metadata):

- `networks`: Compose network key ごとの `network`、`requested_subnet`、`planned_subnet`、`planned_gateway`(省略可)、`relocated`。

atomic write は、state directory に `state.toml.tmp.<pid>.<nanos>` を排他的 create(mode 0600)し、内容の書き込みと fsync の後に `state.toml` へ rename し、最後に親 directory を fsync する実装です。container が存在しない workspace の state は reconcile 時に削除します。

## 10. config hash 入力の実装構成

config hash の公開契約(含める入力・含めない入力・secret-sensitive value の扱い)は [specification.md 10.3 節](specification.md#103-config-hash)にあります。実装(`src/config/hash.rs`)は次の構成です。

- canonical writer(`src/config/canonical.rs`)で決定論的な正規化 text を構築し、SHA-256 の hex digest にする。先頭に version tag `decune-config-hash-v1` を含める。
- トップレベルの入力 field は version、resolved config、feature locks、CLI flags、internal versions、build 入力、Compose 関連(compose files digest、generated override semantic hash 入力、sanitized canonical Compose model。Compose モードのときだけ)、resolved mounts、startup command、UID/GID sync 入力。
- resolved config のうち `ports`(forwarding は up 実行時の runtime 設定)と `container.cli.enabled`(daemon の query policy にだけ影響)は書き込み時に明示的に除外する。
- secret-sensitive value は生値を canonical text に入れず、`${localEnv:...}` 由来の containerEnv / build args は domain 付き SHA-256 marker へ、remoteEnv は固定 marker へ置換する。置換規則の契約は specification.md 10.3 節。
- internal versions は Feature layer 生成と entrypoint shim 生成の内部 version tag で、decune 側の生成 logic が変わったときに既存 container を rebuild 対象へ倒すために使う。
