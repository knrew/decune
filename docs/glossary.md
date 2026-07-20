# decune 用語集

この用語集は、decune のドキュメントで使う用語と表記基準を定義します。README と docs を編集するときは、ここにある語を優先してください。ドキュメント全体の構成と執筆規約は [development.md](development.md#ドキュメント構成と執筆規約) を参照してください。

## 記載基準

用語の各節に載せる語は、次のいずれかに該当し、かつ複数のドキュメントで使われる語とします。

- decune が導入した固有の概念・機能名。
- 一般的な用法や外部仕様(Dev Containers、Docker、Docker Compose)の用法と意味・範囲が異なる、または decune が限定した意味で使う用語。
- decune 内に似た概念が併存し、混同しやすい用語。対になる語を揃えて載せ、区別が分かる定義を書く。

次の内容は載せません。

- 外部仕様の用語で、decune が標準的な意味のまま使うもの。
- 単一の文書内でしか使わない術語。その文書内で定義します。
- 挙動の説明。各語の定義は 1〜2 文と正本へのリンクまでとし、挙動の正は [specification.md](specification.md) に置きます。

「表記ルール」節は、用語の定義とは別に、ドキュメント全体で表記を統一するための規約を定義します。

## CLI 用語

- attached `decune up` session: シェル接続を維持したまま実行中の `decune up`。port forwarding、credential forwarding、container 内からの query はこの session の間だけ有効。
- diagnostic code: 起動前検査や planning の失敗を識別する安定した code(例: `compose_published_port_collision`)。定義は [specification.md 13 章](specification.md#13-diagnostic-code)、対処は [ports.md](ports.md) と [clone-isolation.md](clone-isolation.md)。

## Dev Container 用語

- Dev Container configuration: decune が起動対象にする、`devcontainer.json` を起点とした構成一式の総称。`image` を使う image-based、`build.dockerfile` を使う Dockerfile-based、`dockerComposeFile` と `service` を使う Docker Compose-based の 3 分類がある。
- lifecycle command: `postCreateCommand` や `postAttachCommand` などの Dev Container lifecycle command。
- hook: lifecycle stage の前後に実行する decune-specific command。

## ワークスペースと設定の用語

- workspace root: decune が対象にするローカルプロジェクトディレクトリ。Git リポジトリ内ではリポジトリルート。
- workspace id: workspace root から導出する decune の安定した識別子。Docker resource name と生成データの単位に使う([specification.md 10.1 節](specification.md#101-workspace-id-と-resource-name))。
- decune config: `devcontainer.json` に重ねる decune の TOML overlay 設定。global decune config(`$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`)と project decune config(`<workspace>/.decune/config.toml`)の総称([specification.md 5 章](specification.md#5-decune-config))。
- config hash: decune が管理する既存のコンテナまたは Compose プロジェクトを再利用できるか判定する content hash。
- 生成データ (generated data): decune が XDG cache/state/runtime 配下に生成し、管理しているデータ。workspace 単位の cache / state / runtime data と共有 Feature archive cache を含み、workspace file である `.decune/config.toml` や `.decune/features.lock.toml` は含めない。
- Feature archive cache: OCI Feature archive を再利用するための共有 cache。`$XDG_CACHE_HOME/decune/features` または `~/.cache/decune/features`。
- Feature lock: OCI Feature の解決結果を `<workspace>/.decune/features.lock.toml` に digest lock として記録・固定する仕組み([specification.md 7.1 節](specification.md#71-build-と-features))。

## Docker と Compose の用語

- primary container: image-based / Dockerfile-based configuration の development container、または Compose primary service の container。decune が shell attach、lifecycle、runtime tool を適用する主対象。
- primary service: Dev Container `service` property で選ばれた Compose service。decune が shell attach や lifecycle command などの適用対象にする service([specification.md 8 章](specification.md#8-docker-compose-モード))。
- sidecar service: primary service 以外の Compose service。
- generated Compose override: decune が state/runtime area に生成する Compose override file。利用者は編集しない。
- clone isolation: 同じ Docker Compose-based workspace の複数 clone を同一 Docker daemon 上で同時利用するため、clone-sensitive な published port、固定名、固定 subnet、endpoint を workspace ごとに分離する opt-in 機能。使い方は [clone-isolation.md](clone-isolation.md)。
- clone isolation preflight: 他 clone / 他 workspace と衝突しうる固定名・固定 subnet・stale な endpoint を `docker compose up` の前に検出する処理([specification.md 8.9.2 節](specification.md#892-preflight))。

## ネットワーク用語

- port forwarding: container-side forward agent を経由して host listen address から container port へ転送する decune の機能。`forwardPorts`、decune `[[ports]]`、CLI `-p` は port forwarding。
- forward agent: port forwarding の container 側を担うために decune が container 内で起動する process([specification.md 9 章](specification.md#9-ポート))。
- published port: Docker が host port と container port を publish する設定。image-based / Dockerfile-based configuration では Dev Container `appPort`、Docker Compose-based configuration では Compose service `ports` で指定する。
- fixed TCP published port: host 側 port を明示した TCP の Compose published port entry(例: `3000:3000`)。published port mapping / relocation と clone isolation の分離対象になる単位([specification.md 8.8 節](specification.md#88-published-port-mapping-と-relocation))。
- requested endpoint: 利用者設定や Compose file が要求した host 側 endpoint。
- planned endpoint: decune が起動前に割り当てる予定の host 側 endpoint。Compose published port relocation では requested endpoint と異なる場合がある。
- actual binding: Docker が実際に publish している host 側 binding。`decune ports --json` の `actual_bindings` で確認できる。
- automatic published port relocation: fixed TCP published host endpoint が使用できない場合に、decune が次の利用可能な host port を自動探索する policy(短縮形: automatic relocation)。
- explicit mapping (explicit published port mapping): `[[compose.published_ports.mappings]]` で `service + protocol + target` に対応する planned host endpoint を明示する設定。automatic relocation policy とは独立して適用する。
- endpoint 宣言: 固定 network address を参照する service environment を、Compose network key と relocation 後の gateway / subnet placeholder に対応付ける `[[compose.clone_isolation.endpoints]]` の宣言([specification.md 8.9.4 節](specification.md#894-endpoint-宣言))。
- automatic port forwarding: primary container 内の TCP listening port を検出して decune が転送する機能。`forwardPorts`、decune `[[ports]]`、CLI `-p` による利用者指定の forwarding は manual port forwarding。

## セキュリティ用語

- credential forwarding: host の Git credentials、SSH agent access、GitHub CLI token access を container で利用可能にする仕組み。
- host daemon: `decune up` の子タスクとして動き、`up` process が生きている間だけ credential forwarding、port forwarding support、attached session の container CLI query を担当する process。
- container CLI: primary container 内へ `/run/decune/decune` として配置され、通常は `/usr/local/bin/decune` symlink から実行する container-side client。
- container CLI query: container 内の decune CLI が host daemon 経由で status / ports などの read-only 情報を問い合わせる仕組み。
- query context: host daemon が container CLI query の対象として起動時に固定する、検証済み workspace id と固定 server path の集合。live config や client input からは再解決しない。
- secret-sensitive value: `containerEnv`、`remoteEnv`、`build.args` などで `${localEnv:...}` から来たため、decune が sensitive として追跡する value。
- セキュリティ境界 (security boundary): host と container の間で decune が何を expose し、何を expose しないかを定義する境界([specification.md 12 章](specification.md#12-セキュリティ境界))。

## 表記ルール

- decune CLI documentation では `flag` ではなく `option` を使う。
- `subcommand` は実装説明で必要な場合だけ使い、利用者向けドキュメントでは `command` を使う。
- 仕様上の概念を指すときは `Dev Container`、利用者が作業するコンテナ自体を指す本文では `development container` と書く。
- Docker Compose v2 プラグインを指して `docker-compose` と書かない。`docker-compose` は旧 standalone binary を指す場合だけ使う。
- decune 独自機能の対応範囲は世代番号で表さず、対応・非対応の挙動を直接記述する。protocol の wire version と永続形式・domain separator の識別子はこの表記ルールの対象外。
- `host` と `container` を名詞として使い、local side / remote side のような独自の言い換えを増やさない。
- `port forwarding` と `published port` を明確に分ける。bare `publish` は Docker behavior が文脈で明確な場合だけ使う。
- 利用者向け本文では `Docker Compose-based configuration` を優先し、実装挙動を説明するときだけ `Compose モード` を使う。
- `primary service` は定義後に使う。quick start の本文では必要に応じて「`service` で指定した service」と書く。
