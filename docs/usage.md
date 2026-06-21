# decune の使い方

この文書は、`decune` のインストール方法と利用手順をまとめた利用者向けガイドです。正確な公開挙動、設定スキーマ、セキュリティ境界は [specification.md](specification.md) を参照してください。

## インストール

公式の導入手順は GitHub Releases のビルド済みアーカイブです。インストールスクリプトはバージョンを明示指定し、ダウンロードしたアーカイブを `SHA256SUMS` で検証します。

```sh
mkdir -p "$HOME/.local/bin"
curl -fsSL https://raw.githubusercontent.com/knrew/decune/v0.1.0/scripts/install.sh | sh -s -- --version 0.1.0 --dir "$HOME/.local/bin"
```

`$HOME/.local/bin` が `PATH` に含まれていない場合は、利用しているシェルの設定で追加してください。

### 手動アーカイブインストール

ホストに合う target triple を選び、チェックサムを検証してからインストールします。

```sh
version=0.1.0
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
- Git 認証情報転送を使う場合: ホスト側の `git`
- GitHub CLI token 転送を使う場合: ホスト側の `gh`

必要な Docker Compose の機能は [specification.md](specification.md#ホスト要件) を参照してください。

## クイックスタート

### Image-Based

対象リポジトリに Dev Container 構成を用意します。

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

Compose モードでは Compose サービスの実行時設定を Docker Compose に委譲します。workspace の bind mount は primary service の `volumes` に、Docker published port は Compose サービスの `ports` に書いてください。

## コマンド

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

`WORKSPACE` の既定値はカレントディレクトリです。Git リポジトリ内ではリポジトリルートを workspace root として扱います。

### `decune up`

```sh
decune up
```

development container を作成または起動し、remote user のシェルに接続します。config hash が一致する、decune が管理する起動済みコンテナまたは Compose プロジェクトがある場合は、作成処理をスキップして接続します。

主なオプション:

- `--config <PATH>`: `devcontainer.json` を選択する。decune TOML の重ね合わせ設定ではありません。
- `--detach`: シェルに接続せず、起動だけ行う。
- `--rebuild`: decune が管理するコンテナまたは Compose プロジェクトを再作成する。decune が管理する volume は保持します。
- `--no-cache`: Dockerfile、Compose サービス、Feature layer の build cache を使わない。
- `--pull`: build/create 前に base image または Compose サービス image を pull する。
- `--no-global-config`: global decune TOML 設定を適用しない。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `-p, --port <SPEC>`: 手動の port forwarding 設定を追加する。

`--detach` では `up` 終了時に host daemon も停止するため、manual/automatic port forwarding と Git HTTPS host-helper forwarding は維持されません。detached container で Docker published port が必要な場合は、image/Dockerfile モードでは `appPort`、Compose モードでは Compose サービスの `ports` を使ってください。

### `decune rebuild`

```sh
decune rebuild --no-cache
```

development container または Compose プロジェクトを再作成します。`--update-features` を指定すると、既存の Feature lock より registry/tag の再解決を優先します。

### `decune down`

```sh
decune down
```

decune が管理するコンテナまたは Compose プロジェクトを停止します。volume、状態、image は保持します。

### `decune status`

```sh
decune status
decune status path/to/workspace
```

`decune status` は、state file と decune が付けた Docker label から見つかる workspace environment の summary を表示します。summary には workspace id、workspace path、runtime/config/health、active forwarded/published port count、issue count、`last_used_at` 由来の last-used 表示が含まれます。`last_used_at` がない場合は `-` です。

`WORKSPACE` を指定すると、その workspace の detail を表示します。devcontainer metadata が存在し、まだ decune environment が作成されていない場合は `not-created` として success します。devcontainer metadata が見つからない場合は error です。detail には summary、config file、issue、Compose service、runtime container、ports、resource count、未完了 lifecycle がある場合の lifecycle 状態、必要な action を表示します。

`status` は read-only command です。state の `last_used_at` は更新せず、resource の修復や削除も行いません。JSON 出力や追加の detail option は提供しません。

### `decune ports`

```sh
decune ports
decune ports path/to/workspace
decune ports --all
decune ports --json
decune ports --all --json
```

decune が管理している workspace について、現在有効な host 側 port の利用状況を表示します。`forwardPorts`、decune `[[ports]]`、CLI `-p`、automatic forwarding による port forwarding と、image/Dockerfile モードの `appPort`、Compose サービス `ports` による Docker published port を同じ一覧で確認できます。`TYPE` は `forwarded` または `published` です。

通常出力では単一 workspace で `LOCAL`、`TYPE`、`TARGET`、`SOURCE`、`REQUESTED`、`LABEL` を表示します。`--all` では `WORKSPACE` と `ID` も表示します。host port が使用中で forwarding が別 port に fallback した場合、`REQUESTED` に要求 endpoint を表示します。Docker published port は Docker の実 binding を正として表示するため、`REQUESTED` は `-` です。

`--json` を付けると、通常出力の table を再構成できる JSON array を出力します。各 entry は `host_ip`、`host_port`、`type`、`service`、`container_port`、`protocol`、`source`、`label` を持ち、必要に応じて `workspace`、`workspace_id`、`requested_host_ip`、`requested_host_port` を含みます。現在有効な host 側 port がない場合、通常出力は単一 workspace で `No active ports for this workspace`、`--all` で `No active ports`、JSON 出力は `[]` です。

### `decune remove` / `decune rm`

```sh
decune remove --no-confirm
decune rm --no-confirm
decune remove --all-workspaces --no-confirm
```

指定した workspace に対応する、decune が管理する Dev Container 環境を削除します。対象は decune が管理するコンテナまたは Compose プロジェクト、decune が管理する volume、decune の状態ファイルと実行時ファイルです。`--images` を付けると decune が生成した image も削除します。Compose モードでは利用者が Compose file で指定した image を削除しません。

`--all-workspaces` は、Docker label と decune state から見つかるすべての workspace について、通常の `remove` と同じ削除処理を適用します。Docker label 上または state directory 名の workspace id が decune の有効な形式でない resource / state は無視します。state file と runtime file は削除し、workspace cache と共有 Feature archive cache は削除しません。`--all-workspaces` と `WORKSPACE` は同時に指定できません。

`--no-confirm` は確認プロンプトだけを省略します。削除対象は decune が管理するリソースに限定され、利用者が管理する image / volume を削除しない挙動は変わりません。

### `decune clean`

```sh
decune clean --dry-run
decune clean --no-confirm
decune clean --include-feature-cache --no-confirm
decune clean --dry-run --json
```

stale な decune の生成データを workspace id 単位で確認・削除します。既定の対象は workspace cache、state、runtime data です。active な runtime socket / lock がある workspace や、Docker label から decune が管理している再利用可能なリソースが見つかる workspace は削除せずスキップします。

`--include-feature-cache` は、既定の cleanup 対象に共有 Feature archive cache (`$XDG_CACHE_HOME/decune/features`) を追加します。この option は Feature cache だけを掃除する指定ではありません。既定の `clean` は共有 Feature archive cache を削除しません。

`--dry-run` は削除せず候補を表示します。`--no-confirm` は確認プロンプトだけを省略し、使用中または再利用可能な workspace の保護や symlink traversal の拒否は迂回しません。

`--json` は `dry_run`、`include_feature_cache`、`summary`、`targets` を持つ JSON object を stdout に出力します。`summary` は `remove_candidates`、`removed`、`skipped` を持ちます。workspace target は `kind = "workspace"`、`workspace_id`、`action`、`reason`、`removed`、`paths`、`existing_paths` を持ち、Feature cache target は `kind = "feature_cache"`、`action`、`reason`、`removed`、`path` を持ちます。

## decune TOML 設定

decune TOML の重ね合わせ設定は以下の順で読み込まれます。基本は後勝ちですが、一部の field は仕様で定義した merge rule に従います。

1. decune default
2. image metadata の `devcontainer.metadata`
3. Feature metadata
4. global decune config: `$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`
5. `devcontainer.json`
6. project decune config: `<workspace>/.decune/config.toml`
7. CLI options

`version = 1` は必須です。未知の key は error です。

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
```

完全なスキーマと merge rule は [specification.md](specification.md#decune-toml-設定) を参照してください。

## ポートフォワーディングと published port

`forwardPorts`、decune `[[ports]]`、CLI `-p` は decune のポートフォワーディングです。Docker published port ではありません。既定ではホスト側 `127.0.0.1` で listen し、container-side agent 経由で container port へ転送します。container 内で localhost にだけ listen しているプロセスにも届きます。

`appPort` は image-based / Dockerfile-based 構成の Docker published port です。コンテナ作成時に決まるため、既存コンテナへ後付けできません。

Docker Compose-based 構成では Docker published port を Compose サービスの `ports` に書きます。Dev Container `appPort` は Compose モードでは unsupported error です。

decune port forwarding と、Dev Container `appPort` から decune が生成する published port metadata は TCP-only です。これらの設定で `/udp` を指定すると unsupported error です。Compose サービス `ports` などで Docker が実際に publish している UDP binding は、`decune ports` の一覧に表示されます。

## 認証情報とセキュリティ

`decune up` は Dockerfile instruction、Compose サービス build、local/OCI Feature `install.sh`、lifecycle command、hook、`userEnvProbe` 対象シェル起動ファイルを実行し得ます。信頼していないリポジトリでは、起動前に `.devcontainer/`、Compose file、local Feature、mount、credentials、`privileged`、`capAdd`、`securityOpt`、`appPort`、Compose `ports` を確認してください。

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

## 既知の制限

- Compose モードでは `workspaceMount`、`appPort`、`runArgs` を generated Compose override へ変換しません。
- Compose sidecar service への port forwarding は `forwardPorts` の service syntax または `[[ports]].service` で明示します。
- Dockerfile-based モードでは Dockerfile が build context 配下にある必要があります。
- UDP forwarding と、Dev Container `appPort` から生成する UDP published port metadata には対応しません。
