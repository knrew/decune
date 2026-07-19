# decune 用語集

この用語集は、decune のドキュメントで使う用語と表記基準を定義します。README と docs を編集するときは、ここにある語を優先してください。ドキュメント全体の構成と執筆規約は [development.md](development.md#ドキュメント構成と執筆規約) を参照してください。

## CLI 用語

- command: `up`、`rebuild`、`down`、`remove` など、decune の操作名。
- option: `--detach`、`--no-cache`、`-p` など、名前付きの指定。decune の CLI ドキュメントでは `flag` ではなく `option` を使う。
- argument: `WORKSPACE` のような位置引数、または `--config <PATH>` の `PATH` のような option value。
- subcommand: 実装説明で必要な場合だけ使う。利用者向けドキュメントでは原則 `command` を使う。
- usage: CLI またはリファレンスに示すコマンド構文。
- attached `decune up` session: シェル接続を維持したまま実行中の `decune up`。port forwarding、credential forwarding、container 内からの query はこの session の間だけ有効。
- diagnostic code: 起動前検査や planning の失敗を識別する安定した code(例: `compose_published_port_collision`)。定義は [specification.md 13 章](specification.md#13-診断コード)、対処は [ports.md](ports.md) と [clone-isolation.md](clone-isolation.md)。

## Dev Container 用語

- Dev Container: metadata と container/orchestrator configuration で定義される development environment の仕様上の概念。
- development container: 利用者が作業するコンテナを指す本文用の表現。仕様上の概念を強調しない導入文で使う。
- `devcontainer.json`: decune が読む JSONC metadata file。
- Dev Container configuration: `devcontainer.json`、image metadata、Feature metadata、decune TOML の重ね合わせ設定、CLI options を merge した configuration。
- image-based configuration: `image` を使う Dev Container configuration。
- Dockerfile-based configuration: `build.dockerfile` を使う Dev Container configuration。
- Docker Compose-based configuration: `dockerComposeFile` と `service` を使う Dev Container configuration。
- Feature: OCI registry または local Feature directory から取得する Dev Container Feature。
- lifecycle command: `postCreateCommand` や `postAttachCommand` などの Dev Container lifecycle command。
- hook: lifecycle stage の前後に実行する decune-specific command。

## ワークスペースと設定の用語

- workspace root: decune が対象にするローカルプロジェクトディレクトリ。Git リポジトリ内ではリポジトリルート。
- workspace id: workspace root から導出する decune の安定した識別子。Docker resource name と workspace data の単位に使う([specification.md 10.1 節](specification.md#101-workspace-id-と-resource-name))。
- global decune config: `$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`。
- project decune config: `<workspace>/.decune/config.toml`。
- configuration layer: 最終的に merge される設定の入力。image metadata、Feature metadata、global config、project config、CLI options など。
- config hash: decune が管理する既存のコンテナまたは Compose プロジェクトを再利用できるか判定する content hash。
- generated data: decune が XDG cache/state/runtime 配下に生成し、管理している workspace data や共有 Feature archive cache。workspace file である `.decune/config.toml` や `.decune/features.lock.toml` は含めない。
- workspace data: workspace id 単位で作られる decune の cache、state、runtime data。
- Feature archive cache: OCI Feature archive を再利用するための共有 cache。`$XDG_CACHE_HOME/decune/features` または `~/.cache/decune/features`。
- skeleton fallback: dotfiles の source を直接の bind mount で表現できない場合に、decune が state 領域に生成した skeleton 構造経由で dotfile entry を提供する方式([specification.md 5.7 節](specification.md#57-dotfiles))。
- decune internal path: container 内で decune が予約する `/opt/decune` と `/run/decune` 配下の path。user-defined mount と workspace mount の target には使えない。

## Docker と Compose の用語

- Docker Compose v2 plugin: `docker compose` CLI プラグイン。旧 standalone binary を指すとき以外は `docker-compose` と書かない。
- Compose file: `dockerComposeFile` で参照する Docker Compose YAML file。
- Compose project: 同じ project name のもとで Docker Compose が管理する services、networks、volumes。
- service: Docker Compose service。
- primary container: image/Dockerfile-based 構成の development container、または Compose primary service の container。decune が shell attach、lifecycle、runtime tool を適用する主対象。
- primary service: Dev Container `service` property で選ばれた Compose service。decune が shell attach や lifecycle command などの適用対象にする service([specification.md 8 章](specification.md#8-docker-compose-モード))。
- sidecar service: primary service 以外の Compose service。
- generated Compose override: decune が state/runtime area に生成する Compose override file。利用者は編集しない。
- clone isolation: 同じ Docker Compose-based workspace の複数 clone を同一 Docker daemon 上で同時利用するため、clone-sensitive な published port、固定名、固定 subnet、endpoint を workspace ごとに分離する opt-in 機能。使い方は [clone-isolation.md](clone-isolation.md)。
- clone isolation preflight: 他 clone / 他 workspace と衝突しうる固定名・固定 subnet・stale な endpoint を `docker compose up` の前に検出する処理([specification.md 8.9.2 節](specification.md#892-preflight))。

## ネットワーク用語

- port forwarding: container-side forward agent を経由して host listen address から container port へ転送する decune の機能。`forwardPorts`、decune `[[ports]]`、CLI `-p` は port forwarding。
- published port: Docker が host port と container port を publish する設定。image/Dockerfile モードでは Dev Container `appPort`、Compose モードでは Compose service `ports` で指定する。
- fixed TCP published port: host 側 port を明示した TCP の Compose published port entry(例: `3000:3000`)。published port mapping / relocation と clone isolation の分離対象になる単位([specification.md 8.8 節](specification.md#88-published-port-mapping-と-relocation))。
- requested endpoint: 利用者設定や Compose file が要求した host 側 endpoint。
- planned endpoint: decune が起動前に割り当てる予定の host 側 endpoint。Compose published port relocation では requested endpoint と異なる場合がある。
- automatic published port relocation: fixed TCP published host endpoint が使用できない場合に、decune が次の利用可能な host port を自動探索する policy。
- explicit published port mapping: `[[compose.published_ports.mappings]]` で `service + protocol + target` に対応する planned host endpoint を明示する設定。automatic relocation policy とは独立して適用する。
- subnet pool: clone isolation が固定 IPv4 subnet の workspace 固有 relocation 先を選ぶために利用する IPv4 CIDR 範囲。
- endpoint 宣言: 固定 network address を参照する service environment を、Compose network key と relocation 後の gateway / subnet placeholder に対応付ける `[[compose.clone_isolation.endpoints]]` の宣言([specification.md 8.9.4 節](specification.md#894-endpoint-宣言))。
- actual binding: Docker が実際に publish している host 側 binding。
- availability probe: host port の空き状況を確認するために decune process が行う TCP bind probe。
- unprobeable: availability probe が権限などの理由で空き・占有を判別できない状態。occupied や available とは区別する。
- automatic port forwarding: primary container 内の TCP listening port を検出して decune が転送する機能。
- manual port forwarding: `forwardPorts`、decune `[[ports]]`、CLI `-p` による利用者指定の forwarding。

## セキュリティ用語

- credential forwarding: host の Git credentials、SSH agent access、GitHub CLI token access を container で利用可能にする仕組み。
- host daemon: `decune up` の子タスクとして動き、`up` process が生きている間だけ credential forwarding、port forwarding support、attached session の container CLI query を担当する process。
- container CLI: primary container 内へ `/run/decune/decune` として配置され、通常は `/usr/local/bin/decune` symlink から実行する container-side client。
- container CLI query: container 内の decune CLI が host daemon 経由で status / ports などの read-only 情報を問い合わせる仕組み。
- config snapshot: container CLI の status が、起動時に記録した state と query 時の managed runtime evidence の整合を示す情報。live workspace config の再読込結果ではない。
- query context: host daemon が container CLI query の対象として起動時に固定する、検証済み workspace ID と固定 server path の集合。live config や client input からは再解決しない。
- context fingerprint: query context から domain separation 付き SHA-256 で導出する digest。daemon reuse の同一性比較にだけ使い、raw path を含まない。
- secret-sensitive value: `containerEnv`、`remoteEnv`、`build.args` などで `${localEnv:...}` から来たため、decune が sensitive として追跡する value。
- security boundary: host と container の間で decune が何を expose し、何を expose しないかを定義する境界。

## 表記ルール

- decune CLI documentation では `flag` ではなく `option` を使う。
- decune 独自機能の対応範囲は世代番号で表さず、対応・非対応の挙動を直接記述する。protocol の wire version と永続形式・domain separator の識別子はこの表記ルールの対象外。
- `host` と `container` を名詞として使い、local side / remote side のような独自の言い換えを増やさない。
- `port forwarding` と `published port` を明確に分ける。bare `publish` は Docker behavior が文脈で明確な場合だけ使う。
- 利用者向け本文では `Docker Compose-based configuration` を優先し、実装挙動を説明するときだけ `Compose モード` を使う。
- `primary service` は定義後に使う。quick start の本文では必要に応じて「`service` で指定した service」と書く。
