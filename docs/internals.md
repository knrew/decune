# decune 内部設計ノート

この文書は、`decune` の内部実装の現状をコントリビューター向けに説明する参考情報です。公開挙動の正ではありません。挙動の契約は [specification.md](specification.md) にあり、この文書と実装が specification.md と矛盾する場合は specification.md と実装を正として、この文書を追随させます。

この文書に書く上限値、タイムアウト、TTL などの数値は、公開契約ではなく執筆時点の実装定数です。変更しても仕様変更ではなく、この文書の更新として扱います。検証手順と執筆規約は [development.md](development.md) を参照してください。

## 1. Cargo workspace 構成

Cargo workspace は 4 つの crate で構成します。

- `decune`(リポジトリ直下): ホスト側 CLI 本体。単一バイナリ `decune` を提供する。
- `crates/decune-container-protocol`: decune host daemon と container-side tools が共有する wire protocol 型のライブラリ。
- `crates/decune-container-tools`: コンテナ側の 3 バイナリ。
- `xtask`: 開発・リリース用の自動化タスク。

`decune-container-protocol` は、プロトコルバージョン定数(`HOST_DAEMON_PROTOCOL_VERSION = 1`)、request type(`credential` / `cliQuery`)、daemon error code の文字列定数、request / response の envelope 型(`GitCredentialHostRequest`、`CliQueryRequest`、`HostDaemonResponse`、`HostDaemonError`)、port forward agent のスキャン用 request / response 型を定義します。依存は `serde` だけで、ホスト側(`src/host/`)と container-side tools の両方が同じ型で wire format を読み書きします。

`decune-container-tools` のバイナリは `decune-container-cli`、`decune-forward-agent`、`git-credential-decune` の 3 つです。`clap` に依存せず、コンテナ内 CLI は `args_os` を手続き的にパースします(`src/bin/decune_container_cli/parser.rs`)。bundle への格納時に `decune-container-cli` は artifact 名 `decune` へ改名します(`xtask/src/container_tools.rs` の対応表)。コンテナ内 `/run/decune/decune` の実体はこのバイナリであり、ホスト側の `decune` バイナリではありません。

crate 間の依存は次の形です。

- `decune` と `decune-container-tools` は `decune-container-protocol` にパス依存する。
- `decune` は `decune-container-tools` にコンパイル時依存しない。container tools はビルドスクリプト(`build.rs`)が bundle のファイルとして埋め込む(6 節)。
- `xtask` は workspace 内の crate に依存せず、`cargo` / `rustup` を子プロセスとして呼ぶ。

ホスト側の `decune` バイナリは、argv[0] のファイル名が `git-credential-decune` または `decune-forward-agent` に一致する場合、そのツールとして動作する分岐を `src/main.rs` に持ちます。本体 crate 側にも同等の helper / agent 実装があり(`src/host/credentials/`、`src/host/forward/`)、ホスト側のテストはこの実装を in-process 実行にも使います。コンテナへ配置する artifact は `decune-container-tools` の独立したバイナリです。

`xtask` のサブコマンドは `build-container-tools` / `check-container-tools` / `install` / `dist` / `checksum` / `release-manifest` / `release-preflight` / `workspace-test` / `compose-integration` です。使い方は [development.md](development.md) にあります。

## 2. CLI アダプター構成

外部コマンドは `std::process::Command` / `tokio::process::Command` に argv の配列を渡して実行します。これは [specification.md 12.2 節](specification.md#122-外部コマンド実行と-redaction)の argv 実行原則の実装形です。

アダプターは次の 3 つです。

- `DockerCli`: `docker` の存在確認、version、image/container/exec/cp/inspect/build/pull/rm/stop/start/wait/port 相当の操作。
- `DockerComposeCli`: `docker compose` の存在確認、`version --short`、必須機能の確認、config/build/up/stop/down/ps/logs/pull 相当の操作。
- `RuntimeCommand`: コマンド実行、stdout/stderr のキャプチャ、ストリーミング、exit status、タイムアウト、シグナル処理、redaction の共通基盤。

JSON を読む操作は、次の CLI の JSON 出力を serde の型へパースします。

- `docker image inspect --format json` または `docker inspect --format json`
- `docker compose config --format json`
- `docker compose ps --format json`

argv 実行原則、エラー変換、redaction の契約は specification.md 12.2 節、Podman 互換性は [2.4 節](specification.md#24-podman-互換性)にあります。

## 3. decune host daemon の実装

decune host daemon の接続 / クエリの受け入れ制御と I/O の固定上限は次のとおりです(`src/host/daemon.rs`、`src/host/query.rs` の実装定数)。

| 項目 | 現在値 |
| --- | --- |
| active な decune host daemon の接続数 | 32 |
| request body 上限 | 64 KiB |
| request body の読み取りタイムアウト | 2 s |
| response の書き込みタイムアウト | 2 s |
| active な `cliQuery` | 8 |
| `cliQuery` の合計タイムアウト | 15 s |

接続の permit は listener から accept してタスクを生成する前に確保します。上限中は新しい接続タスクを生成せず、接続は listener / OS の backlog 側で待たせます。credential、予約済みの request、`cliQuery` を含む全接続を同じ上限で数え、peer UID の検証、プロトコルバージョンの検証、request body の 64 KiB 上限を適用します。request body を 2 s 以内に EOF まで読めない場合は接続を閉じ、response の `write_all` と書き込み側の shutdown は合わせて 2 s 以内に完了させます。

decune host daemon の停止時は accept ループと受理済みの接続タスクを中断します。accept の失敗で accept ループだけが終了した場合は、新規接続の受付を止め、処理中の接続タスクは中断せず完了まで待ちます。読み取り / 書き込みのタイムアウト、I/O エラー、daemon 停止によるタスク中断では完全な wire response を保証せず、クライアントは transport のエラーとして扱います([specification.md 3.9 節](specification.md#39-コンテナ内の-decune-cli))。

`cliQuery` は envelope、厳格なスキーマ、ポリシー、コマンド / format の組み合わせをすべて検証した後にだけクエリ用セマフォへ入れます。credential の request はクエリ用セマフォを使いません。8 件の permit は待機せず `try_acquire` で取得し、上限時は即座に `cli_query_busy` を返します。15 s の期限は permit 取得後から状態、forwarding、Docker / キャッシュ待機、render、response 構築、JSON へのシリアライズまでを含み、ソケット書き込みの 2 s のタイムアウトは含みません。期限超過は `cli_query_timeout`、その他の致命的な collector / render / serialization の失敗は `cli_query_failed` になります。いずれのエラーの response にも部分的な出力と警告を含めません(error code の定義は [specification.md 13.3 節](specification.md#133-decune-host-daemon-error-code))。

daemon query context の fingerprint は、`decune-cli-query-context-v1` で domain separation した SHA-256 digest です。決まったフィールド順で workspace id と固定サーバーパスのコンテキストだけを入力にし、秘密情報、トークン、credential の値、解決済み設定の全体を入力に含めません。decune host daemon のメタデータ(ランタイムディレクトリの `host-daemon.json`)にはクエリポリシーと context fingerprint だけを保存し、生のコンテキスト、ホスト側パス、秘密情報を保存しません。daemon の再利用の identity は `Disabled` または `Enabled { context_fingerprint }` の 2 値です。

## 4. decune container CLI query の collector と evidence cache

コンテナクエリの collector は、daemon 起動時に固定したサーバー側コンテキストだけを入力にします。状態は固定の状態ディレクトリの `state.toml` をクエリごとに 1 回だけ読み、ワークスペースパスや設定パスを参照先として使いません。forwarding status も固定の status ディレクトリの全セッションのソケットをクエリごとに 1 回だけ集約します(8.4 節)。decune host daemon は `ForwardStatusRegistry` を所有または注入されず、daemon の所有者と forwarding セッションの所有者が異なる場合も共有の status ディレクトリから全セッションを検出します。1 つのセッションが停止した場合は、残るセッションだけが次のクエリに反映されます。`Workspace::resolve`、設定の探索、read-only の up 計画、ビルドコンテキストのハッシュは呼び出しません。

Docker のコンテナ evidence は、固定 workspace id の `decune.managed=true` リソースと、固定の状態または同リソースから導出した同一 Compose プロジェクトだけを列挙 / inspect / 重複排除します。Compose プロジェクトのラベルの候補は、固定の状態に記録された値、または `decune.managed=true` かつ `decune.workspace_id` が固定 workspace id と一致するリソースの値に限定します。ラベルの値は前後の空白を除いた後に空でないことだけを確認し、Compose プロジェクト名の形式検証は行いません。ここで確認しているのはラベルの文字列形式ではなく、固定の daemon query context または同一ワークスペースに帰属する decune 管理リソースから得た値であることです。request のコマンド、format、パス、リソース名は Docker のフィルタやホスト側パスに使いません。生の inspect、生のラベルマップ、stdout / stderr はコンテナクエリの許可リスト型へ直ちに射影し、キャッシュへ保存しません。status と ports はコンテナ / サービス / 実行状態 / ヘルス / 設定の identity / published port を含む同じコンテナ evidence のスナップショットを共有し、decune 管理ボリュームの evidence は別のエントリとして取得します。

Docker evidence のキャッシュのキーはサーバー側だけで次の値から作ります。

```text
QueryEvidenceKey {
    query_context_fingerprint,
    workspace_id,
    kind: Containers | Volumes,
}
```

クライアント入力、ワークスペースパス、Docker のリソース名、出力の format はキーに含めません。`Containers` はワークスペースのコンテナと同一ワークスペースの Compose プロジェクトのコンテナの意味単位の読み込み全体、`Volumes` は decune 管理ボリュームの evidence を表します。状態と forwarding status はキャッシュしません。

キャッシュとクエリ専用の Docker 実行の内部固定値は次のとおりです(`src/host/query.rs` の実装定数)。

| 項目 | 現在値 |
| --- | --- |
| 同時 Docker evidence 読み込み | 2 |
| Docker evidence 読み込みのタイムアウト | 10 s |
| クエリ用 Docker コマンドのタイムアウト | 5 s |
| 成功キャッシュの TTL | 2 s |
| 失敗キャッシュの TTL | 500 ms |

TTL は読み込み完了時刻から数えます。同一キーのコールドな読み込みは意味単位の読み込み全体を singleflight し、待機側は同じ型付きの成功またはサニタイズ済みの型付きの失敗を共有します。異なるキーを含め、実行中の Docker evidence の読み込みは全体で 2 件までです。キャッシュヒットした Docker evidence は読み込み完了時点から最大 2 s stale になり得ます(公開契約としての縮退の記述は [specification.md 12.5 節](specification.md#125-decune-container-cli-query-の境界))。期限切れの成功の再読み込みが失敗した場合に stale な結果は返しません。Docker のイベント監視や変更フックによる無効化は行わず、daemon の再生成時にキャッシュを破棄します。

Docker evidence の読み込みはクエリの coordinator が独立したタスクとして所有します。呼び出し元のキャンセルだけでは読み込みを中断せず、完了・失敗・10 s のタイムアウトの全経路で待機側を起こします。クエリ専用の Docker コマンドには `RuntimeCommand` のタイムアウト / kill / reap を使って 5 s のタイムアウトを設定し、通常のホスト側 `status` / `ports` / `up` のコマンドのタイムアウトは変更しません。Docker の失敗は生の stderr を保持しない型付きの失敗へ変換した後、collector の縮退規則に従って警告付きのスナップショットへ変換します。クエリ全体の 15 s の期限と daemon の受け入れ制御は daemon の dispatch が所有します(3 節)。

記録済みの状態と実行時の設定の identity の比較には生のハッシュを保持せず、domain separation した digest へ射影した非表示の比較値を使います。この比較値は `Debug` とシリアライズに出力しません。

## 5. コンテナ内 CLI の transport 内部

コンテナ内 CLI の transport は、daemon handoff 中のソケット交換を許容するため、connect の `NotFound` / `ConnectionRefused` だけを再試行します。接続試行は合計最大 5 回、試行間隔は 100 ms です(初回 1 回 + 再試行 4 回。`crates/decune-container-tools/src/bin/decune_container_cli/transport.rs`)。response の上限 1 MiB、再試行しないエラーの種別、canonical unavailable error は公開契約として [specification.md 3.9 節](specification.md#39-コンテナ内の-decune-cli)にあります。

## 6. container tools bundle と実行時の配置

container-side tools はリリースビルド時にホストのバイナリへ埋め込みます。bundle は `git-credential-decune`、`decune-forward-agent`、`decune`(Cargo の binary target は `decune-container-cli`)の 3 ツールを各プラットフォームに 1 artifact ずつ持ち、現在の 2 プラットフォーム(`linux-amd64` = `x86_64-unknown-linux-musl`、`linux-arm64` = `aarch64-unknown-linux-musl`)で 6 artifact になります。bundle のディレクトリには、スキーマのバージョンとプロトコルバージョン、artifact ごとの名前 / プラットフォーム / パス / SHA-256 を記録した `manifest.json` を置きます。

ビルド時の埋め込みは `build.rs` が行い、次の内部環境変数で制御します(7 節)。

- `DECUNE_CONTAINER_TOOLS_BUNDLE`: `auto`(既定。bundle のディレクトリに `manifest.json` があれば検証して埋め込み、なければ埋め込みなし)、`required`(bundle 必須。欠落・検証失敗はビルドエラー)、`off`(埋め込みなし)。
- `DECUNE_CONTAINER_TOOLS_BUNDLE_DIR`: bundle のディレクトリの場所。既定は `target/decune-xtask/container-tools-bundle`。

`cargo run --locked -p xtask -- install --locked` は bundle をビルド / 検証し、`DECUNE_CONTAINER_TOOLS_BUNDLE=required` を設定した `cargo install --profile dist` で bundle 埋め込みの `decune` をインストールします。通常のローカル / CI 手順ではこれらの環境変数を直接使わず、`xtask` が内部で設定します。

開発・デバッグ用の実行時の上書きとして `DECUNE_CONTAINER_TOOLS_DIR` があります。設定すると、埋め込みの bundle の代わりに指定ディレクトリの bundle(`manifest.json` 必須)を検証して使います。埋め込みなしのビルドで上書きも未設定の場合、container tools を必要とする操作はエラーになります。

container-side tool の実行時の配置は、コンテナにマウントするランタイムディレクトリ内へ一時ファイルを作りません。ホスト専用かつ配置先と同一ファイルシステムの親ディレクトリに排他的作成で一時ファイルを作り、開いたファイルディスクリプタへの artifact のバイト列の書き込み、モード `0755` の設定、最終的に配置するバイト列の SHA-256 検証が完了した後、実行時の配置先をアトミックな rename で置換します。既存の配置先が symlink の場合はリンク先を変更せず symlink のエントリ自体を置換し、ディレクトリなど安全に置換できないファイル種別はランタイム領域の破損としてエラーにします。失敗時は一時ファイルを削除し、部分的な配置先を公開しません(公開契約は [specification.md 11 章](specification.md#11-配布の契約))。

## 7. 内部環境変数

次の環境変数は decune 内部の契約で、公開契約ではありません。名前と意味は予告なく変わり得ます。

ホスト側の `decune` が読むもの:

- `DECUNE_DOCKER_RESOURCE_LOCK`: Docker リソース操作をプロセス間で直列化する flock 用ファイルのパス。未設定ならロックは無効。並行テスト / CI 向けの内部の回避手段(`src/docker/lock.rs`)。
- `DECUNE_CONTAINER_TOOLS_DIR`: 埋め込みの bundle の代わりに使う外部の container tools bundle のディレクトリ(6 節)。

ホストがコンテナ側プロセスへ渡すもの:

- `DECUNE_FORWARD_AGENT_SOCKET` / `DECUNE_FORWARD_AGENT_SECRET` / `DECUNE_FORWARD_AGENT_ALLOWED_PORTS`: port forward agent を `docker exec` で起動するときの環境変数。socket は agent が待ち受けるコンテナ内の Unix ソケットのパス、secret はホストと agent の接続認証に使うランダムな 16 進値(redaction 登録済みで、agent は起動後に自プロセスの環境から除去する)、allowed ports は転送を許可するコンテナのポートのカンマ区切りリスト。
- `DECUNE_REMOTE_USER` や `DECUNE_GH_CONFIG_OWNER` など、decune が生成する exec 用スクリプト向けの補助変数。
- decune-generated Compose override は `${DECUNE_CONTAINER_ENV_<KEY>}` 形式のプレースホルダーを参照し、secret-sensitive な `containerEnv` の値を override のファイルに直書きせず子プロセスの環境変数として渡す。

container-side tools が読むもの:

- `DECUNE_HOST_DAEMON_SOCKET`: `decune-container-cli` と `git-credential-decune` の接続先ソケットの上書き。未設定時は既定の `/run/decune/host-daemon.sock` を使う。通常経路では設定されず、テスト / デバッグ用。

ビルド時に `build.rs` が読むもの:

- `DECUNE_CONTAINER_TOOLS_BUNDLE` / `DECUNE_CONTAINER_TOOLS_BUNDLE_DIR`(6 節)。

このほかに、ビルドスクリプトがバージョン表示用に設定する rustc-env(`DECUNE_DISPLAY_VERSION` など)と、テスト専用の変数(`DECUNE_TEST_*`、`DECUNE_E2E_*` など)がありますが、実行時の内部契約ではないためこの一覧には含めません。

## 8. ホスト側ディレクトリと生成ファイルのレイアウト

### 8.1 ディレクトリの解決

workspace id は、正規化した workspace root のパスの SHA-256 digest の先頭 12 桁の 16 進文字です(`src/workspace.rs`)。ワークスペース単位のディレクトリは次の場所に作ります。

| ディレクトリ | 場所 | XDG 未設定時のフォールバック |
| --- | --- | --- |
| 状態ディレクトリ | `$XDG_STATE_HOME/decune/<workspace_id>` | `~/.local/state/decune/...` |
| ランタイムディレクトリ | `$XDG_RUNTIME_DIR/decune/<workspace_id>` | `/tmp/decune-<uid>/<workspace_id>` |
| キャッシュディレクトリ | `$XDG_CACHE_HOME/decune/<workspace_id>` | `~/.cache/decune/...` |
| Feature archive cache | `$XDG_CACHE_HOME/decune/features` | `~/.cache/decune/features` |

ランタイムディレクトリの親(`decune` / `decune-<uid>`)とランタイムディレクトリ自体は 0700 に設定・検証します(`src/host/runtime.rs`)。

### 8.2 状態ディレクトリ

状態ディレクトリ直下に生成するファイル:

- `state.toml`: ワークスペースの状態(9 節)。
- `state.toml.tmp.<pid>.<nanos>`: アトミックな書き込み用の一時ファイル。rename 後は残らない。
- `compose.override.yaml`: Compose モードで decune が生成する Compose override。

`.decune/features.lock.toml` は状態ディレクトリではなくワークスペース側のファイルで、Feature archive cache はキャッシュ側の共有ディレクトリです。

### 8.3 ランタイムディレクトリ

ランタイムディレクトリは全体をコンテナの `/run/decune` へ bind mount します。配下のファイルと作り手は次のとおりです。

| ファイル | 作り手 | 内容 |
| --- | --- | --- |
| `host-daemon.sock` | decune host daemon | credential / `cliQuery` 用の Unix ソケット |
| `host-daemon.json` | decune host daemon | daemon の再利用メタデータ(クエリポリシーと context fingerprint。0600) |
| `decune` | ホスト(配置) | decune container CLI artifact(実体は `decune-container-cli`) |
| `git-credential-decune` | ホスト(配置) | Git credential helper artifact |
| `host-gitconfig` | ホスト | ホストの `~/.gitconfig` のコピー(`copy_global_config` 有効時。0600) |
| `decune-forward-agent` | ホスト(配置) | port forward agent artifact |
| `forward-agent-<session_id>.sock` | コンテナ内の port forward agent | forwarding セッションの Unix ソケット(単一セッション名は `forward-agent.sock`) |
| `forward-agent.err` | コンテナ内の port forward agent | agent 失敗時の診断メッセージ |
| `forward-agent.status` | コンテナ内の port forward agent | agent の終了 status(ホストが起動失敗の検出に読む) |
| `secrets/github-token` | ホスト | GitHub CLI のトークン(ディレクトリ 0700 / ファイル 0600。コンテナへは read-only でマウント) |
| `feature-entrypoints-complete` | ホスト | Feature entrypoint の完了を伝えるマーカー |
| `feature-entrypoints-token` | ホスト | Feature entrypoint 用のトークン |
| `forward/<service_key>/` | ホスト | Compose の sidecar forwarding 用のサービス固有のランタイムディレクトリ(port forward agent だけを含む) |

コンテナ側だけに現れる関連マウントとして、`/run/decune/gh`(GitHub CLI 設定用の書き込み可能な tmpfs。`GH_CONFIG_DIR` が指す)と `/run/decune/ssh-agent.sock`(ホストの `SSH_AUTH_SOCK` ソケットの bind mount)があります。これらの実体はランタイムディレクトリ内のファイルではありません。

### 8.4 forwarding status directory

forwarding status directory はランタイムディレクトリの兄弟ディレクトリ `<runtime_dir>-ports`(0700)です。forwarding セッションごとにホスト側の status サーバーが次の 2 ファイルを作ります。

- `forward-status-<session_id>.sock`: セッションの forwarding 一覧を返す Unix ソケット(0600)。
- `forward-status-<session_id>.json`: version、セッション id、ソケット名、pid を持つメタデータ(0600)。

集約(`decune ports` のホスト側表示や decune container CLI query の forwarding 情報)は、status ディレクトリのメタデータファイルを列挙・整列し、各ソケットへ `list` の request を送って全セッションの forwarding を連結します。connect が `NotFound` / `ConnectionRefused` になるソケットは stale として黙ってスキップし、その他の失敗は警告にします。セッションの停止時はソケットとメタデータを削除し、次回の集約から自然に消えます。decune host daemon はこの仕組みだけで全セッションを発見するため、forwarding セッションの所有プロセスと daemon の所有者が異なっても集約できます(4 節)。

### 8.5 コンテナ内のその他の内部パス

- `/opt/decune/dotfiles` と `/opt/decune/dotfile-backings`: dotfiles のマウント先と backing のマウント。
- `/opt/decune/cache`: decune 管理のキャッシュのマウント。
- `/usr/local/share/decune/feature-entrypoint-wrapper.sh`: Feature entrypoint の wrapper。ランタイムディレクトリではなくイメージのビルド時に焼き込む。

`/run/decune` と `/opt/decune` が利用者定義のマウント先として使えない制約は [specification.md](specification.md) にあります。

### 8.6 旧形式のパス

GitHub トークンの旧形式のランタイムパス `gh-token/token`(コンテナ側 `/run/decune/gh-token`)は、新規の配置には使いません。現行実装(`src/host/credentials/github.rs`、`src/up/existing.rs`)では削除対象として扱い、旧形式のマウントを持つ既存コンテナを stale とみなして再作成させる判定にだけ使います。公開契約のパスは `/run/decune/secrets/github-token` だけです。

## 9. state.toml のキー構成

状態ファイルの公開挙動は [specification.md 10.4 節](specification.md#104-状態ファイル)が定める互換性契約(version、未知のモードの読み込み、アトミックな書き込み、ラベル優先)だけで、全キーの構成は内部形式です。serde の構造体(`src/state.rs`)は未知のフィールドを拒否します。

トップレベルのキー:

| キー | 内容 |
| --- | --- |
| `version` | 状態の version。現在 `1` |
| `workspace` | workspace root の表示用パス |
| `mode` | 起動時のモードのスナップショット(`image` / `dockerfile` / `compose`。フィールドがない状態は `unknown`) |
| `container_id` | primary container の ID |
| `image` | 起動に使ったイメージ |
| `config_hash` | reuse hash(10 節) |
| `config_file` | `--config` で指定した `devcontainer.json` のパス(省略可) |
| `compose_project_name` | Compose のプロジェクト名(Compose モードのみ) |
| `created_at` | 作成時刻(`unix:<seconds>` 形式) |
| `last_started_at` | 最終起動時刻(同上) |
| `last_used_at` | 最終利用時刻(同上。ない状態もある) |
| lifecycle 完了フラグ | `on_create_completed` / `after_on_create_completed` / `update_content_completed` / `after_update_content_completed` / `post_create_completed` / `after_post_create_completed` の 6 つの真偽値 |

`[[published_ports]]`(Compose published port relocation の表示補助メタデータ):

- `source`(現在 `compose` のみ)、`type`(現在 `published` のみ)、`service`、`port_entry_index`。
- `target`: `port` と `protocol`。
- `requested` / `planned`: `host_ip_kind`(`omitted` / `explicit`)、`host_ip_value`(explicit 時のみ)、`host_port`。
- `actual_bindings`: 起動時に Docker inspect で観測した `host_ip` / `host_port` の配列。
- `relocated`: relocation が起きたかどうか。

`[clone_isolation]`(network relocation の表示補助メタデータ):

- `networks`: Compose のネットワークキーごとの `network`、`requested_subnet`、`planned_subnet`、`planned_gateway`(省略可)、`relocated`。

アトミックな書き込みは、状態ディレクトリに `state.toml.tmp.<pid>.<nanos>` を排他的に作成(モード 0600)し、内容の書き込みと fsync の後に `state.toml` へ rename し、最後に親ディレクトリを fsync する実装です。コンテナが存在しないワークスペースの状態は整合処理時に削除します。

## 10. reuse hash 入力の実装構成

reuse hash の公開契約(含める入力・含めない入力・secret-sensitive value の扱い)は [specification.md 10.3 節](specification.md#103-reuse-hash)にあります。実装(`src/config/hash.rs`)は次の構成です。

- canonical writer(`src/config/canonical.rs`)で決定論的な正規化テキストを構築し、SHA-256 の 16 進 digest にする。先頭にバージョンタグ `decune-config-hash-v1` を含める。
- トップレベルの入力フィールドは version、解決済み設定、Feature lock、CLI オプション、内部バージョン、ビルド入力、Compose 関連(Compose ファイルの digest、decune-generated Compose override semantic hash の入力、サニタイズ済みの canonical Compose model。Compose モードのときだけ)、解決済みマウント、起動コマンド、UID/GID 同期の入力。
- 解決済み設定のうち `ports`(forwarding は `up` 実行時の実行時設定)と `container.cli.enabled`(daemon のクエリポリシーにだけ影響)は書き込み時に明示的に除外する。
- secret-sensitive value は生の値を正規化テキストに入れず、`${localEnv:...}` 由来の `containerEnv` / `build.args` は domain 付きの SHA-256 マーカーへ、`remoteEnv` は固定マーカーへ置換する。置換規則の契約は specification.md 10.3 節。
- 内部バージョンは Feature レイヤー生成と entrypoint shim 生成の内部バージョンタグで、decune 側の生成ロジックが変わったときに既存コンテナを再作成対象へ倒すために使う。
