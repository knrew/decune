# decune

`decune` は、VS Code や Node.js ベースの Dev Container CLI に依存せず、Rust 製の単一 CLI で devcontainer を起動・接続・停止・削除するためのツールです。

Dev Containers Specification の image-based / Dockerfile-based / Docker Compose-based 構成を読み込み、Docker CLI / Docker Compose CLI 経由で container または Compose project を操作します。加えて、個人用・プロジェクト用の TOML 設定、Dev Container Features、dotfiles、Git/GitHub 認証、localhost port forwarding、UID/GID sync を扱います。

## 特徴

- `decune up` で devcontainer を作成・起動し、remote user の shell に接続
- `decune rebuild` / `decune down` / `decune clean` による明示的な lifecycle 管理
- `.devcontainer/devcontainer.json`、`.devcontainer.json`、`.devcontainer/<name>/devcontainer.json` の検出
- image-based / Dockerfile-based / Docker Compose-based devcontainer の起動
- Docker Compose の `dockerComposeFile`、`service`、`runServices` 対応
- `~/.config/decune/config.toml` と `<workspace>/.decune/config.toml` の重ね合わせ
- OCI / local Dev Container Features、feature lock、Feature metadata merge
- Git HTTPS credential helper、SSH agent、GitHub CLI token の forwarding
- VS Code の `forwardPorts` に近い、container localhost 宛て port forwarding
- Linux host での `updateRemoteUserUID` による UID/GID sync
- GitHub Releases 用の prebuilt archive 配布

## 現在のスコープ

v0.1 は image-based / Dockerfile-based / Docker Compose-based devcontainer を対象にします。Docker Compose mode では、Dev Container の primary service に対して Features、dotfiles、credentials、lifecycle、remote shell、port forwarding を適用します。

未対応または意図的に対象外の主な項目は以下です。

- 旧 `docker-compose` v1 standalone binary の公式対応
- Kubernetes、Swarm stack、Docker Desktop UI、cloud provider 固有 orchestrator の直接サポート
- VS Code 拡張機能のインストールと `customizations.vscode` の適用
- GPG agent forwarding
- コンテナから任意の host command を実行する API
- Windows host 向け公式配布
- `cargo install` / `cargo install --git` を公式インストール手段として扱うこと

詳細な仕様は [docs/specification.md](docs/specification.md) を参照してください。

## 必要なもの

- Linux または macOS host
- Docker CLI `docker`
- Docker Compose v2 plugin（`docker compose version` が成功し、以下の capability があること）
  - `docker compose config --format json`
  - `docker compose ps --format json`
  - `docker compose build --with-dependencies`
  - `docker compose pull --policy always`
  - `docker compose pull --ignore-buildable`
  - `docker compose pull --include-deps`
  - `docker compose up --force-recreate`
  - `docker compose up --remove-orphans`
- Docker daemon へ接続できる権限
- Git 認証連携を使う場合: host 側の `git`、必要に応じて `SSH_AUTH_SOCK`
- GitHub CLI 連携を使う場合: host 側の `gh` と `gh auth token` が成功する状態

## インストール

公式導線は GitHub Releases の prebuilt archive です。archive には `decune` binary、`LICENSE`、`README.md` が含まれます。

```sh
# 例: release asset を取得して展開する
curl -L -o decune.tar.gz \
  https://github.com/knrew/decune/releases/download/v0.1.0/decune-v0.1.0-x86_64-unknown-linux-musl.tar.gz

tar -xzf decune.tar.gz
sudo install -m 0755 decune-v0.1.0-x86_64-unknown-linux-musl/decune /usr/local/bin/decune

decune --help
```

配布 archive の checksum は同じ Release に含まれる `SHA256SUMS` で確認します。

```sh
sha256sum -c SHA256SUMS
```

開発用に source checkout から動かす場合は、container-side tools bundle が必要になる機能があります。Git credential helper と port forward agent を使う場合は、bundle を生成してから build してください。

```sh
cargo run --locked -p xtask -- build-container-tools --out assets/container-tools --locked
cargo build --release --locked
```

軽い `cargo check` だけを行う場合は、通常の Cargo command を実行できます。

```sh
cargo check --workspace --all-targets --all-features
```

## Quick start

### image-based

対象 repository に `devcontainer.json` を用意します。

```jsonc
// .devcontainer/devcontainer.json
{
  "name": "example",
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
  "remoteUser": "vscode",
  "features": {
    "ghcr.io/devcontainers/features/github-cli:1": {}
  },
  "forwardPorts": [5173],
  "postCreateCommand": "echo ready"
}
```

### Dockerfile-based

Dockerfile を build する場合は `build.dockerfile` を指定します。`build.options` は Docker build の argv として渡されますが、decune が管理する `--file`、`--tag`、`--label`、`--build-arg`、`--target`、`--cache-from`、`--no-cache`、`--pull`、output / metadata file 系 option は指定できません。build context path も decune が管理するため、`build.options` には書けません。

```jsonc
// .devcontainer/devcontainer.json
{
  "name": "dockerfile-example",
  "build": {
    "dockerfile": "Dockerfile",
    "context": "..",
    "options": [
      "--platform=linux/amd64",
      "--ssh=default",
      "--secret",
      "id=npm,env=NPM_TOKEN",
      "--network",
      "host"
    ]
  },
  "remoteUser": "vscode"
}
```

`build.options` の値は argv に出るため、secret 文字列そのものを直接書かないでください。`--secret id=npm,env=NPM_TOKEN` のように host 環境変数や file path を参照する形にしてください。

### Docker Compose-based

Docker Compose を使う場合は、`dockerComposeFile` と primary `service` を指定します。

```jsonc
// .devcontainer/devcontainer.json
{
  "name": "compose-example",
  "dockerComposeFile": "compose.yaml",
  "service": "app",
  "runServices": ["app", "db"],
  "workspaceFolder": "/workspaces/example",
  "features": {
    "ghcr.io/devcontainers/features/github-cli:1": {}
  },
  "forwardPorts": ["app:5173", "db:5432"],
  "postCreateCommand": "echo ready"
}
```

```yaml
# .devcontainer/compose.yaml
services:
  app:
    image: mcr.microsoft.com/devcontainers/base:ubuntu
    volumes:
      - ..:/workspaces/example:cached
    command: sleep infinity
  db:
    image: postgres:16
    environment:
      POSTGRES_PASSWORD: postgres
```

必要に応じて project-local の decune 設定を追加します。

```toml
# .decune/config.toml
version = 1
shell = "/bin/bash"

[[ports]]
container = 5173
host = 5173
host_ip = "127.0.0.1"
label = "web"

[credentials.git]
enabled = true
https = "host-helper"
ssh_agent = "auto"

[credentials.github]
enabled = true
mode = "gh-token-file"
install_feature_if_missing = true
```

起動します。

```sh
decune up
```

コンテナまたは Compose project を再作成します。

```sh
decune rebuild --no-cache
```

停止します。volume と state は残します。

```sh
decune down
```

decune 管理リソースを削除します。

```sh
decune clean --force
```

## コマンド

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

`WORKSPACE` の既定値はカレントディレクトリです。Git repository 内では repository root を workspace root として扱います。

### `decune up`

```sh
decune up [OPTIONS] [WORKSPACE]
```

devcontainer を作成または起動し、remote user の shell に接続します。image/Dockerfile mode では単一 container、Compose mode では Compose project を起動します。既に起動済みで設定 hash が一致する container/project があれば、作成処理をスキップして接続します。

主なオプション:

- `--config <PATH>`: devcontainer metadata file を明示する。decune TOML overlay の指定ではありません。
- `--detach`: shell に接続せず起動だけ行う。
- `--rebuild`: 既存 container/project を破棄して再作成する。decune 管理 volume は保持する。
- `--no-cache`: Dockerfile build、Compose service build、Feature layer build で cache を使わない。
- `--pull`: base image または Compose service image を pull してから build/create する。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `-p, --port <SPEC>`: manual forwarding。例: `3000`, `3000:3000`, `127.0.0.1:8080:3000`, `[::1]:8080:3000`。

`--detach` では host daemon も `up` 終了時に止まるため、manual/automatic forwarding と Git HTTPS host-helper は維持されません。detached container で外部公開したい port は、image/Dockerfile mode では `appPort`、Compose mode では Compose file の `ports` を使ってください。`--detach -p` はエラーになります。

### `decune rebuild`

```sh
decune rebuild [OPTIONS] [WORKSPACE]
```

`up --rebuild` と同等の明示サブコマンドです。`--update-features` を指定すると、既存の feature lock より registry/tag の再解決を優先します。

主なオプション:

- `--detach`: shell に接続せず起動だけ行う。
- `--no-cache`: Dockerfile build、Compose service build、Feature layer build で cache を使わない。
- `--pull`: base image または Compose service image を pull してから build/create する。
- `--update-features`: feature lock より registry/tag の再解決を優先する。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `-p, --port <SPEC>`: manual forwarding。`--detach -p` はエラーになります。

### `decune down`

```sh
decune down [--timeout <SECONDS>] [WORKSPACE]
```

decune 管理 container または Compose project を停止します。volume、state、image は保持します。

### `decune clean`

```sh
decune clean [--force] [--images] [WORKSPACE]
```

decune 管理 container / Compose project、volume、state/runtime を削除します。`--images` を付けた場合だけ decune generated image も削除します。Compose mode では user が Compose file で指定した image を削除しません。

TTY でない環境では確認プロンプトを出せないため、`--force` なしの `clean` はエラーになります。

## 設定ファイル

decune TOML は以下の順で読み込まれます。後勝ちが基本です。

1. decune default
2. image metadata の `devcontainer.metadata`
3. Feature metadata
4. global decune config: `$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`
5. `devcontainer.json`
6. project decune config: `<workspace>/.decune/config.toml`
7. CLI flags

最小例:

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
# Compose mode で sidecar service を対象にする場合だけ指定する。
# service = "db"

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

`version = 1` は必須です。未知の key は typo とみなしエラーにします。

## Port forwarding と publish

`forwardPorts`、decune TOML の `[[ports]]`、CLI の `-p` は decune forwarding です。host 側の既定 listen address は `127.0.0.1` で、container 内の `127.0.0.1:<port>` にだけ listen している開発 server にも届くよう、container-side agent 経由で転送します。

`appPort` は image/Dockerfile mode の Docker publish です。container 作成時に決まるため、既存 container への後付けはできません。host IP を省略した publish は Docker の既定により localhost 限定にならない可能性があります。localhost 限定が必要な場合は `forwardPorts`、`[[ports]]`、または `decune up -p` を使ってください。

CLI `-p` と `appPort` の IPv6 host IP は `[::1]:8080:3000` のような bracketed form で指定します。unbracketed IPv6 は曖昧なためエラーになります。

Compose mode の publish は Compose file の `ports` に委譲します。Compose sidecar service へ forwarding する場合は、`forwardPorts` の `"service:port"` 形式、または decune TOML の `[[ports]].service` を使います。

## 認証とセキュリティ

`decune up` は devcontainer の Dockerfile、Compose service build、Feature `install.sh`、lifecycle command、hook、`userEnvProbe` 対象 shell startup file を実行し得ます。信頼していない repository では、実行前に `.devcontainer/`、Compose file、local Feature、mount、credentials、`privileged`、`capAdd`、`securityOpt`、`appPort` / Compose `ports` を確認してください。

untrusted repository では、認証 forwarding を無効にする設定を推奨します。

```toml
version = 1

[credentials.git]
enabled = false

[credentials.github]
enabled = false
```

GitHub CLI 連携を有効にすると、host の `gh auth token` から得た token は一時 file として container に read-only mount されます。token は Docker label、container env、state、config hash、image layer には保存しませんが、container 内プロセスからは token file に到達できます。

`containerEnv` は container 作成時の環境変数です。container 内プロセスや Docker inspect から見えるため、decune は `containerEnv` を secret storage として扱いません。`${localEnv:VAR}` から展開された `containerEnv` / `remoteEnv` / `build.args` の値は state、config hash、generated Compose override、argv、通常の error 表示に平文保存しないよう redaction します。`containerEnv` と `build.args` は config hash に平文ではなく非可逆 digest として変更検出情報を含め、host 側の値が変わった既存 container / Compose project は再利用しません。Docker build arg は image layer や build output に残る可能性があるため、build secret には Docker BuildKit secret を使ってください。`runArgs`、`workspaceFolder`、`remoteUser`、`containerUser` も secret storage ではありません。literal に書かれた secret 文字列は decune が secret と判定できません。

Dev Container `runArgs` は Docker option の完全 pass-through ではなく allowlist です。image/Dockerfile mode では `--init`、`--privileged`、`--cap-add`、`--security-opt`、`--add-host`、`--dns`、`--dns-search`、`--network`、`--network-alias`、`--hostname`、`--device`、`--group-add`、`--ulimit`、`--ipc`、`--shm-size`、`--gpus` を受け付けます。`--mount`、`--volume`、`--env`、`--env-file`、`--publish`、`--user`、`--workdir`、`--entrypoint`、`--label`、`--name` など decune が管理する option は拒否されます。Compose mode では `runArgs` は unsupported のため、Compose service の field に書いてください。

SSH agent forwarding は `SSH_AUTH_SOCK` を `/run/decune/ssh-agent.sock` として container に渡します。`ssh_agent = "required"` の場合、host 側 socket が使えないと `up` は失敗します。

## 開発

通常の検証:

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

`compose_integration` filter の Docker-backed test は `#[ignore]` として定義します。通常の unit test では実行されず、`cargo run --locked -p xtask -- compose-integration` が Docker daemon と Docker Compose v2 plugin を確認したうえで `cargo test --workspace --all-features --no-fail-fast compose_integration -- --ignored --test-threads=1` を実行します。

配布成果物の生成:

```sh
cargo run --locked -p xtask -- dist \
  --target x86_64-unknown-linux-musl \
  --version 0.1.0 \
  --locked

cargo run --locked -p xtask -- checksum --dist-dir target/dist --version 0.1.0
cargo run --locked -p xtask -- release-manifest --dist-dir target/dist --version 0.1.0
```

## ドキュメント

- [docs/specification.md](docs/specification.md): v0.1 の共有仕様、設定、セキュリティ境界、検証方針

## License

MIT License. See [LICENSE](LICENSE).
