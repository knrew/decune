# decune の使い方

この文書は、`decune` のインストールと日常操作をまとめた利用者向けの基本ガイドです。設定の使い方は [configuration.md](configuration.md)、ポートの利用は [ports.md](ports.md)、複数 clone の同時利用は [clone-isolation.md](clone-isolation.md)、正確な公開挙動、設定スキーマ、セキュリティ境界は [specification.md](specification.md) を参照してください。

## インストール

公式の導入手順は GitHub Releases のビルド済みアーカイブです。インストールスクリプトはバージョンを明示指定し、ダウンロードしたアーカイブを `SHA256SUMS` で検証します。

```sh
mkdir -p "$HOME/.local/bin"
curl -fsSL https://raw.githubusercontent.com/knrew/decune/v0.3.4/scripts/install.sh | sh -s -- --version 0.3.4 --dir "$HOME/.local/bin"
```

`$HOME/.local/bin` が `PATH` に含まれていない場合は、利用しているシェルの設定で追加してください。

decune を upgrade する前に、対象 workspace で動いている attached `decune up` session をすべて終了してください。binary を更新した後、新しい version で `decune up` を起動し直します。旧/new host daemon と container-side client の mixed-version compatibility は保証されません。

### 手動アーカイブインストール

ホストに合う target triple を選び、チェックサムを検証してからインストールします。

```sh
version=0.3.4
target=x86_64-unknown-linux-musl
archive="decune-v${version}-${target}.tar.gz"
base="https://github.com/knrew/decune/releases/download/v${version}"

curl -L -O "$base/$archive"
curl -L -O "$base/SHA256SUMS"
grep "  $archive$" SHA256SUMS > SHA256SUMS.selected
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c SHA256SUMS.selected
else
  shasum -a 256 -c SHA256SUMS.selected
fi

tar -xzf "$archive"
sudo install -m 0755 "decune-v${version}-${target}/decune" /usr/local/bin/decune

decune --help
```

対応する target:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

ソースコードからのインストールには、`Cargo.toml` の `rust-version` 以上の Rust stable が必要です。手順は [development.md](development.md#ソースからのローカルインストール) を参照してください。

## 要件

- Linux または macOS ホスト
- Docker CLI `docker`
- Docker Compose v2 プラグイン
- Docker デーモンへ接続できる権限
- Git credential forwarding を使う場合: ホスト側の `git`
- GitHub CLI token forwarding を使う場合: ホスト側の `gh`

必要な Docker Compose の機能は [specification.md](specification.md#21-ホスト要件) を参照してください。

## クイックスタート

### Image-Based

対象リポジトリに Dev Container configuration を用意します。

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

起動して remote user のシェルに接続します。

```sh
decune up
```

### Dockerfile-Based

Dockerfile から development container を build する場合は `build.dockerfile` を指定します。

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

`build.options` の値はプロセス引数に出ます。secret の実値は書かず、`--secret id=npm,env=NPM_TOKEN` のように Docker BuildKit secret reference を使ってください。

`build.dockerfile` は解決後の `build.context` 配下にある必要があります。context 外の Dockerfile を使う場合は、Dockerfile を context 内に移動するか、`build.context` を Dockerfile を含むディレクトリへ広げてください。

### Docker Compose-Based

Docker Compose を使う場合は `dockerComposeFile` と `service` を指定します。

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

Docker Compose-based configuration では Compose service の実行時設定を Docker Compose に委譲します。workspace の bind mount は `service` で指定した service の `volumes` に、Docker published port は Compose service の `ports` に書いてください(ポートの使い分けは [ports.md](ports.md))。

## コマンド

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

`WORKSPACE` の既定値はカレントディレクトリです。Git リポジトリ内ではリポジトリルートを workspace root として扱います。各コマンドの正確な契約と全オプションは [specification.md 3 章](specification.md#3-cli) を参照してください。

### `decune up`

development container を作成または起動し、remote user のシェルに接続します。起動済みで設定が変わっていなければ、作成処理をスキップして接続だけ行います。

```sh
decune up
```

よく使う操作:

- シェルに接続せず起動だけ行う: `decune up --detach`。port forwarding と credential forwarding は維持されません(ポートへの影響は [ports.md](ports.md#--detach-とポート))。
- `devcontainer.json` を明示する: `decune up --config .devcontainer/other/devcontainer.json`
- global decune config を適用しない: `decune up --no-global-config`
- port forwarding を追加する: `decune up -p 8080:3000`(使い方は [ports.md](ports.md#manual-port-forwarding))

設定変更を既存の container / Compose project に反映できない場合、`up` は暗黙に rebuild せず、`decune rebuild` を促して終了します。

### `decune rebuild`

development container または Compose プロジェクトを再作成します。設定変更を反映するときや、image を作り直したいときに使います。decune が管理する volume は保持されます。

```sh
decune rebuild
decune rebuild --no-cache          # build cache を使わずに build し直す
decune rebuild --pull              # base image / Compose service image を pull し直す
decune rebuild --update-features   # Feature lock より registry/tag の再解決を優先する
```

### `decune down`

decune が管理するコンテナまたは Compose プロジェクトを停止します。volume、状態、image は保持され、次の `decune up` で再利用できます。

```sh
decune down
```

### `decune status`

decune が管理する workspace environment の状態を read-only で表示します。

```sh
decune status                    # 全 workspace の summary
decune status path/to/workspace  # 指定 workspace の detail
```

summary では workspace ごとの実行状態、設定状態、health、port count、issue 数を確認できます。detail では issue の内訳と、必要な操作(`decune rebuild` の要否など)が action として表示されます。表示内容の契約は [specification.md 3.5 節](specification.md#35-status) を参照してください。

### `decune ports`

decune が管理している workspace について、現在有効な host 側 port の利用状況を read-only で表示します。port forwarding と Docker published port の両方が同じ一覧に出ます。

```sh
decune ports          # 現在の workspace
decune ports --all    # workspace 横断
decune ports --json   # JSON 出力
```

表の見方と relocation の確認方法は [ports.md](ports.md#decune-ports-での確認)、列と JSON schema の契約は [specification.md 3.6 節](specification.md#36-ports) を参照してください。

### Container 内の `decune`

active な attached `decune up` session がある間、primary container の中から、現在の workspace に固定された read-only query を実行できます。

```sh
decune status
decune ports
decune ports --json
```

- container 内の `status` は、起動時に記録した state と query 時の managed Docker resource の比較を表示します。host 側のような live config check は行わず、`Live workspace: not checked` と表示されます。host で行う操作が必要な場合は `Action (run on host)` に表示されます。
- container 内では workspace を指定できません。`up` / `rebuild` / `down` / `remove` / `clean` などの host-only command も実行できません。
- `up --detach` の完了後や attached session の終了後は、artifact が残っていても query は利用できません。
- `/usr/local/bin/decune` を準備できなかったという warning が host 側に出た場合は、container 内で `/run/decune/decune status` のように direct path を使ってください。

有効/無効の設定は [configuration.md](configuration.md#containercli)、query の契約とセキュリティ境界は [specification.md 3.9 節](specification.md#39-container-内の-decune-cli) を参照してください。

### `decune remove` / `decune rm`

指定した workspace に対応する、decune が管理する Dev Container 環境(コンテナまたは Compose プロジェクト、decune 管理の volume、状態・実行時ファイル)を削除します。利用者が管理する image / volume は削除しません。

```sh
decune remove
decune remove --images                       # decune が生成した image も削除する
decune remove --all-workspaces --no-confirm  # すべての workspace を対象に、確認なしで削除する
```

`--no-confirm` は確認プロンプトだけを省略し、decune が管理するリソースに限定する安全境界は迂回しません。削除範囲の契約は [specification.md 3.7 節](specification.md#37-remove--rm) を参照してください。

### `decune clean`

stale な decune の生成データ(workspace の cache / state / runtime data)を workspace id 単位で確認・削除します。使用中の workspace と、Docker 上に再利用可能なリソースが残っている workspace はスキップされます。

```sh
decune clean --dry-run                             # 削除候補の確認だけ行う
decune clean --no-confirm                          # 確認なしで削除する
decune clean --include-feature-cache --no-confirm  # 共有 Feature archive cache も対象に追加する
decune clean --dry-run --json                      # 削除候補を JSON で確認する
```

既定では共有 Feature archive cache (`$XDG_CACHE_HOME/decune/features`) を削除しません。対象の判定規則と JSON schema の契約は [specification.md 3.8 節](specification.md#38-clean) を参照してください。

## decune config

decune config は `devcontainer.json` に重ねる overlay 設定です。個人環境向けの global config と workspace ごとの project config があり、複数 layer の後勝ちで合成されます。

- global: `$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`
- project: `<workspace>/.decune/config.toml`

最小例:

```toml
version = 1
shell = "/bin/zsh"

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
```

各設定の使い方と挙動は [configuration.md](configuration.md)、ポート関連の設定は [ports.md](ports.md)、clone isolation は [clone-isolation.md](clone-isolation.md)、スキーマと merge rule は [specification.md 5 章](specification.md#5-decune-config) を参照してください。

## 安全な使い方

`decune up` は Dockerfile instruction、Compose service build、local/OCI Feature `install.sh`、lifecycle command、hook、`userEnvProbe` 対象シェル起動ファイルを実行し得ます。信頼していないリポジトリでは、起動前に `.devcontainer/`、Compose file、local Feature、mount、credentials、`privileged`、`capAdd`、`securityOpt`、`appPort`、Compose `ports` を確認してください。

信頼していないリポジトリでは、credential forwarding を無効化するか、Git HTTPS lookup を read-only に制限します。

```toml
version = 1

[credentials.git]
https = "host-helper-read-only"
ssh_agent = "off"

[credentials.github]
enabled = false
```

`host-helper-read-only` は Git credential `get` request だけをホストに forwarding し、`store` / `erase` は success no-op として扱います。SSH agent forwarding は別経路なので、不要な場合は `ssh_agent = "off"` も設定してください。

GitHub CLI integration は一時 token file を read-only で container に mount します。token value は Docker label、container env、state、config hash、generated image、generated Compose override file に保存しませんが、container 内プロセスからは token file に到達できます。

credential 設定の詳細は [configuration.md](configuration.md#credentialsgit)、セキュリティ境界の定義は [specification.md 12 章](specification.md#12-セキュリティ境界) を参照してください。
