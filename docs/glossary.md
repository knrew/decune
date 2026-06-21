# decune 用語集

この用語集は、decune のドキュメントで使う用語と表記基準を定義します。README と docs を編集するときは、ここにある語を優先してください。

## CLI 用語

- command: `up`、`rebuild`、`down`、`remove` など、decune の操作名。
- option: `--detach`、`--no-cache`、`-p` など、名前付きの指定。decune の CLI ドキュメントでは `flag` ではなく `option` を使う。
- argument: `WORKSPACE` のような位置引数、または `--config <PATH>` の `PATH` のような option value。
- subcommand: 実装説明で必要な場合だけ使う。利用者向けドキュメントでは原則 `command` を使う。
- usage: CLI またはリファレンスに示すコマンド構文。

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
- global decune config: `$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`。
- project decune config: `<workspace>/.decune/config.toml`。
- configuration layer: 最終的に merge される設定の入力。image metadata、Feature metadata、global config、project config、CLI options など。
- config hash: decune が管理する既存のコンテナまたは Compose プロジェクトを再利用できるか判定する content hash。
- generated data: decune が XDG cache/state/runtime 配下に生成し、管理している workspace data や共有 Feature archive cache。workspace file である `.decune/config.toml` や `.decune/features.lock.toml` は含めない。
- workspace data: workspace id 単位で作られる decune の cache、state、runtime data。
- Feature archive cache: OCI Feature archive を再利用するための共有 cache。`$XDG_CACHE_HOME/decune/features` または `~/.cache/decune/features`。

## Docker と Compose の用語

- Docker Compose v2 plugin: `docker compose` CLI プラグイン。旧 standalone binary を指すとき以外は `docker-compose` と書かない。
- Compose file: `dockerComposeFile` で参照する Docker Compose YAML file。
- Compose project: 同じ project name のもとで Docker Compose が管理する services、networks、volumes。
- service: Docker Compose service。
- primary service: Dev Container `service` property で選ばれた Compose service。decune は shell attach、lifecycle command、Features、dotfiles、credentials、UID/GID sync、automatic port forwarding をこの service に適用する。
- sidecar service: primary service 以外の Compose service。
- generated Compose override: decune が state/runtime area に生成する Compose override file。利用者は編集しない。

## ネットワーク用語

- port forwarding: container-side forward agent を経由して host listen address から container port へ転送する decune の機能。`forwardPorts`、decune `[[ports]]`、CLI `-p` は port forwarding。
- published port: Docker が host port と container port を publish する設定。image/Dockerfile モードでは Dev Container `appPort`、Compose モードでは Compose service `ports` で指定する。
- automatic port forwarding: primary container 内の TCP listening port を検出して decune が転送する機能。
- manual port forwarding: `forwardPorts`、decune `[[ports]]`、CLI `-p` による利用者指定の forwarding。

## セキュリティ用語

- credential forwarding: host の Git credentials、SSH agent access、GitHub CLI token access を container で利用可能にする仕組み。
- host daemon: `decune up` の子タスクとして動き、`up` process が生きている間だけ credential forwarding と port forwarding support を担当する process。
- secret-sensitive value: `containerEnv`、`remoteEnv`、`build.args` などで `${localEnv:...}` から来たため、decune が sensitive として追跡する value。
- security boundary: host と container の間で decune が何を expose し、何を expose しないかを定義する境界。

## 表記ルール

- decune CLI documentation では `flag` ではなく `option` を使う。
- `host` と `container` を名詞として使い、local side / remote side のような独自の言い換えを増やさない。
- `port forwarding` と `published port` を明確に分ける。bare `publish` は Docker behavior が文脈で明確な場合だけ使う。
- 利用者向け本文では `Docker Compose-based configuration` を優先し、実装挙動を説明するときだけ `Compose モード` を使う。
- `primary service` は定義後に使う。quick start の本文では必要に応じて「`service` で指定した service」と書く。
