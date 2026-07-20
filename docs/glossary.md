# decune 用語集

この用語集は、decune のドキュメントで使う用語を定義します。README と docs を編集するときは、ここにある語を優先してください。ドキュメント全体の構成と執筆規約は [development.md](development.md#ドキュメント構成と執筆規約) を参照してください。

## 記載基準

用語の各節に載せる語は、次のいずれかに該当し、かつ複数のドキュメントで使われる語とします。

- decune が導入した固有の概念・機能名。
- 一般的な用法や外部仕様(Dev Containers、Docker、Docker Compose)の用法と意味・範囲が異なる、または decune が限定した意味で使う用語。
- decune 内に似た概念が併存し、混同しやすい用語。対になる語を揃えて載せ、区別が分かる定義を書く。

次の内容は載せません。

- 外部仕様の用語で、decune が標準的な意味のまま使うもの。
- 単一の文書内でしか使わない術語。その文書内で定義します。
- 挙動の説明。各語の定義は 1〜2 文と正本へのリンクまでとし、挙動の正は [specification.md](specification.md) に置きます。

## CLI 用語

- attached `decune up` session: シェル接続を維持したまま実行中の `decune up`。port forwarding、credential forwarding、コンテナ内からのクエリはこのセッションの間だけ有効。
- diagnostic code: 起動前検査や計画作成の失敗を識別する安定したコード(例: `compose_published_port_collision`)。定義は [specification.md 13 章](specification.md#13-diagnostic-code)、対処は [ports.md](ports.md) と [clone-isolation.md](clone-isolation.md)。

## Dev Container 用語

- Dev Container configuration: decune が起動対象にする、`devcontainer.json` を起点とした構成一式の総称。`image` を使う image-based、`build.dockerfile` を使う Dockerfile-based、`dockerComposeFile` と `service` を使う Docker Compose-based の 3 分類がある。
- 開発コンテナ: 利用者が作業する開発用のコンテナ自体を指す語。仕様上の概念を指すときは Dev Container と書く。
- lifecycle command: `postCreateCommand` や `postAttachCommand` などの Dev Container lifecycle command。
- decune hook: lifecycle stage の前後に実行する decune 固有のコマンド。`[[hooks.*]]` で定義する([specification.md 5.16 節](specification.md#516-hooks))。

## ワークスペースと設定の用語

- workspace root: decune が対象にするローカルプロジェクトディレクトリ。Git リポジトリ内ではリポジトリルート。
- workspace id: workspace root から導出する decune の安定した識別子。Docker リソース名と decune-managed data の単位に使う([specification.md 10.1 節](specification.md#101-workspace-id-とリソース名))。
- decune config: `devcontainer.json` に重ねる decune の TOML オーバーレイ設定。global decune config(`$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`)と project decune config(`<workspace>/.decune/config.toml`)の総称([specification.md 5 章](specification.md#5-decune-config))。
- reuse hash: 構成内容から決定的に導出し、decune が管理する既存のコンテナまたは Compose プロジェクトを再利用できるか判定するハッシュ([specification.md 10.3 節](specification.md#103-reuse-hash))。
- decune-managed data: decune が XDG の cache/state/runtime 配下に生成し、管理しているデータ。ワークスペース単位のキャッシュ / 状態 / ランタイムデータと共有 Feature archive cache を含み、ワークスペース側のファイルである `.decune/config.toml` や `.decune/features.lock.toml` は含めない。
- Feature archive cache: OCI Feature のアーカイブを再利用するための共有キャッシュ。`$XDG_CACHE_HOME/decune/features` または `~/.cache/decune/features`。
- Feature lock: OCI Feature の解決結果を `<workspace>/.decune/features.lock.toml` に digest lock として記録・固定する仕組み([specification.md 7.1 節](specification.md#71-ビルドと-feature))。

## Docker と Compose の用語

- primary container: image-based / Dockerfile-based configuration の開発コンテナ、または Compose primary service のコンテナ。decune がシェル接続、lifecycle command、実行時ツールを適用する主対象。
- primary service: Dev Container の `service` プロパティで選ばれた Compose サービス。decune がシェル接続や lifecycle command などの適用対象にするサービス([specification.md 8 章](specification.md#8-docker-compose-モード))。
- sidecar service: primary service 以外の Compose サービス。
- decune-generated Compose override: decune が状態・ランタイム領域に生成する Compose override のファイル。利用者は編集しない。
- clone isolation: 同じ Docker Compose-based ワークスペースの複数クローンを同一 Docker デーモン上で同時利用するため、クローン間で衝突しうる published port、固定名、固定サブネット、エンドポイントをワークスペースごとに分離するオプトインの機能。使い方は [clone-isolation.md](clone-isolation.md)。
- clone isolation preflight: 他のクローン / 他のワークスペースと衝突しうる固定名・固定サブネット・stale なエンドポイントを `docker compose up` の前に検出する処理([specification.md 8.9.2 節](specification.md#892-preflight))。
- network relocation: clone isolation の機能の一つ。Compose ネットワークの固定 IPv4 サブネットを、`subnet_pool` から割り当てたワークスペース固有のサブネットへ付け替える([specification.md 8.9.3 節](specification.md#893-network-relocation))。
- name rewrite: clone isolation の機能の一つ。明示的なサービスの `container_name` とトップレベルリソースの固定 `name` をワークスペース固有名へ書き換える([specification.md 8.9.5 節](specification.md#895-name-rewrite))。
- `docker-compose`: 旧 v1 の単体バイナリ。decune の対象外で、Docker Compose v2 プラグインを指す表記には使わない。

## ネットワーク用語

- port forwarding: コンテナ側の port forward agent を経由して、ホスト側の待ち受けアドレスからコンテナのポートへ転送する decune の機能。`forwardPorts`、decune `[[ports]]`、CLI `-p` は port forwarding。
- port forward agent: port forwarding のコンテナ側を担うために decune がコンテナ内で起動するプロセス([specification.md 9 章](specification.md#9-ポート))。
- published port: Docker がホスト側ポートとコンテナのポートを publish する設定。image-based / Dockerfile-based configuration では Dev Container `appPort`、Docker Compose-based configuration では Compose サービスの `ports` で指定する。
- fixed TCP published port: ホスト側ポートを明示した TCP の Compose published port のエントリ(例: `3000:3000`)。published port mapping / relocation と clone isolation の分離対象になる単位([specification.md 8.8 節](specification.md#88-published-port-mapping-と-relocation))。
- requested endpoint: 利用者設定や Compose ファイルが要求したホスト側エンドポイント。
- planned endpoint: decune が起動前に割り当てる予定のホスト側エンドポイント。Compose published port relocation では requested endpoint と異なる場合がある。
- actual binding: Docker が実際に publish しているホスト側のバインディング。`decune ports --json` の `actual_bindings` で確認できる。
- automatic published port relocation: fixed TCP published port の requested endpoint が使用できない場合に、decune が次に利用可能なホスト側ポートを自動探索するポリシー(短縮形: automatic relocation)。
- explicit published port mapping: `[[compose.published_ports.mappings]]` で `service + protocol + target` に対応する planned endpoint を明示する設定。automatic relocation のポリシーとは独立して適用する。
- clone isolation endpoint 宣言: 固定のネットワークアドレスを参照するサービスの環境変数を、Compose のネットワークキーと relocation 後のゲートウェイ / サブネットのプレースホルダーに対応付ける `[[compose.clone_isolation.endpoints]]` の宣言([specification.md 8.9.4 節](specification.md#894-clone-isolation-endpoint-宣言))。
- automatic port forwarding: primary container 内で TCP で待ち受けているポートを検出して decune が転送する機能。`forwardPorts`、decune `[[ports]]`、CLI `-p` による利用者指定の転送は manual port forwarding。

## セキュリティ用語

- credential forwarding: ホストの Git 認証情報、SSH agent へのアクセス、GitHub CLI トークンへのアクセスをコンテナで利用可能にする仕組み。
- decune host daemon: `decune up` の子タスクとして動き、`up` のプロセスが生きている間だけ credential forwarding、port forwarding の支援、attached `decune up` session の decune container CLI query を担当するプロセス。
- daemon handoff: decune host daemon を所有する `decune up` session の終了時に、daemon を再利用している別のセッションが同じポリシーと daemon query context で daemon を再起動して引き継ぐ処理([specification.md 12.4 節](specification.md#124-decune-host-daemon))。
- container-side tools: decune がコンテナ内で実行するために配置する 3 ツール(`git-credential-decune`、port forward agent の `decune-forward-agent`、decune container CLI)の総称。リリースビルドでは bundle としてホスト側バイナリへ埋め込む([specification.md 11 章](specification.md#11-配布の契約))。
- decune container CLI: primary container 内へ `/run/decune/decune` として配置され、通常は `/usr/local/bin/decune` の symlink から実行するコンテナ側クライアント。
- decune container CLI query: コンテナ内の decune CLI が decune host daemon 経由で `status` / `ports` などの read-only 情報を問い合わせる仕組み。
- daemon query context: decune host daemon が decune container CLI query の対象として起動時に固定する、検証済みの workspace id と固定サーバーパスの集合。live な設定やクライアント入力からは再解決しない。
- Docker evidence: decune が Docker の列挙 / inspect から取得する、decune-managed コンテナ / ボリュームの観測スナップショット。ホスト側 `status` の判定と decune container CLI query の応答に使う。
- secret-sensitive value: `containerEnv`、`remoteEnv`、`build.args` などで `${localEnv:...}` から来たため、decune が秘密情報として追跡する値。
- セキュリティ境界(security boundary): ホストとコンテナの間で decune が何を公開し、何を公開しないかを定義する境界([specification.md 12 章](specification.md#12-セキュリティ境界))。
