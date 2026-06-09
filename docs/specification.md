# decune v0.1 共有仕様

この文書は、`decune` v0.1 の利用者・貢献者向け共有仕様である。実装作業ログや milestone 履歴ではなく、公開挙動、設定形式、セキュリティ境界、検証方針を記録する。

## 目的

`decune` は、Dev Containers Specification の devcontainer を Rust 製の単一 CLI から起動、接続、停止、削除するためのツールである。VS Code や Node.js ベースの Dev Container CLI には依存しない。Docker Engine API 互換 daemon は原則 bollard 経由で操作し、Compose mode では Docker Compose 固有 semantics を Docker Compose v2 CLI に委譲する。

個人設定と project 設定は TOML で重ねられる。VS Code Dev Containers が暗黙に提供する Git/GitHub 認証、dotfiles、port forwarding も decune の責務として明示的に扱う。

## 参照仕様

- Development Containers Specification: https://github.com/devcontainers/spec
- Dev Container metadata reference: https://containers.dev/implementors/json_reference/
- Dev Container Features reference: https://containers.dev/implementors/features/
- Dev Container CLI: https://github.com/devcontainers/cli
- VS Code Dev Containers: https://code.visualstudio.com/docs/devcontainers/containers
- VS Code Git credentials sharing: https://code.visualstudio.com/remote/advancedcontainers/sharing-git-credentials
- Docker bind mounts: https://docs.docker.com/engine/storage/bind-mounts/
- Docker build context and `.dockerignore`: https://docs.docker.com/build/concepts/context/
- Docker container publish: https://docs.docker.com/reference/cli/docker/container/run/
- bollard crate: https://docs.rs/bollard/latest/bollard/

## v0.1 スコープ

### 実装対象

- Rust 製単一バイナリの CLI。
- Docker Engine API 操作は原則 bollard を使用する。
- Dev Container の image-based、Dockerfile-based、Docker Compose mode 構成。
- Compose mode では Docker Compose v2 CLI を専用 adapter から呼び出し、Compose file merge、variable interpolation、profiles、build、depends_on、healthcheck、network、volume などの Compose 固有 semantics は Docker Compose に委譲する。
- JSONC としての `devcontainer.json` 読み込み。
- TOML による global/project 設定。
- Dev Container Features の OCI registry 取得、digest lock、local Feature、インストール、metadata merge。
- 追加 mount、dotfiles setup、read-only mount、symlink 解決。
- Git HTTPS credential helper、SSH agent、GitHub CLI token forwarding。
- manual port forwarding と automatic port forwarding。
- Linux host での `updateRemoteUserUID` による UID/GID sync。
- lifecycle command と decune 固有 hooks。
- `up`、`rebuild`、`down`、`clean` サブコマンド。
- GitHub Releases の prebuilt archive による公式配布。

### 対象外

- VS Code 拡張機能のインストールや `customizations.vscode` の適用。
- GPG agent forwarding。
- コンテナから任意の host command を実行する API。
- cloud provider や remote Docker host に特化した path forwarding。
- Windows host 向け公式配布。
- `cargo install` / `cargo install --git` を公式インストール手段として扱うこと。

## 配布仕様

公式配布は GitHub Releases の prebuilt archive とする。release archive は以下を含む。

- `decune` binary
- `LICENSE`
- `README.md`

release asset:

- `decune-v{version}-{host_triple}.tar.gz`
- `SHA256SUMS`
- `release-manifest.json`

初期 target:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

container-side tools は release build 時に host binary へ埋め込む。Git repository には生成済み binary artifact を入れない。

container-side tool platform:

- `linux-amd64`
- `linux-arm64`

開発・debug 用 override として `DECUNE_CONTAINER_TOOLS_DIR` を残す。build-time の bundle 制御は `DECUNE_CONTAINER_TOOLS_BUNDLE` と `DECUNE_CONTAINER_TOOLS_BUNDLE_DIR` で行う。

## CLI

共通形式:

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

- `WORKSPACE` の既定値はカレントディレクトリ。
- `WORKSPACE` は実在するディレクトリでなければならない。
- Git repository 内では repository root を workspace root とする。Git repository でなければ指定ディレクトリを workspace root とする。
- v0.1 では `devcontainer.json` を必須とする。decune TOML は overlay であり、base image/build 定義の置き換えには使わない。
- CLI output、log、error text は英語にする。
- 設定変更が既存 container に反映できない場合、`up` は暗黙 rebuild を行わず、`Run decune rebuild` を促して終了する。

### `up`

```text
decune up [OPTIONS] [WORKSPACE]
```

役割:

- devcontainer を作成または起動し、remote user の shell にログインする。
- 既に起動済みなら作成処理をスキップし、shell ログインのみ行う。
- decune host daemon、credential bridge、port forwarder は `up` process が生きている間だけ動作する。

主要 option:

- `--config <PATH>`: devcontainer metadata file を明示する。relative path は workspace root 相対。
- `--detach`: shell に接続せず起動だけ行う。
- `--rebuild`: 既存 container を破棄して再作成する。decune 管理 volume は保持する。
- `--no-cache`: Dockerfile build と Feature layer build で cache を使わない。
- `--pull`: base image を pull してから build/create する。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `-p, --port <SPEC>`: manual forwarding。例: `3000`, `3000:3000`, `127.0.0.1:8080:3000`。複数指定可。

`--detach` では `up` process 終了時に host daemon も停止するため、manual/automatic forwarding と Git HTTPS host-helper は維持されない。detached container で外部公開が必要な port は `appPort` による Docker publish を使う。`--detach` と CLI `-p` / `--port` の併用は error とする。設定由来の `forwardPorts` / `[[ports]]` は warning を出して無視する。

### `rebuild`

```text
decune rebuild [OPTIONS] [WORKSPACE]
```

`up --rebuild` と同等の明示サブコマンドである。既存 container を停止・削除し、再 build/create/start する。decune 管理 volume は保持する。

主要 option:

- `--detach`
- `--no-cache`
- `--pull`
- `--update-features`: feature lock より registry/tag の再解決を優先する。
- `-p, --port <SPEC>`

### `down`

```text
decune down [--timeout <SECONDS>] [WORKSPACE]
```

decune 管理 container を停止する。volume、state、image は削除しない。

### `clean`

```text
decune clean [--force] [--images] [WORKSPACE]
```

managed container、managed volume、state/runtime を削除する。`--images` 指定時だけ generated image を削除する。TTY でない `clean` without `--force` は確認不能として error にする。

## devcontainer.json サポート

### 検出順序

workspace root から以下の順で検出する。

1. `.devcontainer/devcontainer.json`
2. `.devcontainer.json`
3. `.devcontainer/<name>/devcontainer.json`

`--config <PATH>` が指定された場合は自動検出を行わず、その path を devcontainer metadata file として使う。relative path は workspace root 相対で解決する。3 に複数候補がある場合、v0.1 では自動選択せず、`--config .devcontainer/<name>/devcontainer.json` で明示する。

### 対応プロパティ

| property | 対応 | 備考 |
| --- | --- | --- |
| `image` | yes | image-based mode |
| `build.dockerfile` | yes | Dockerfile-based mode |
| `build.context` | yes | `devcontainer.json` からの相対 path |
| `build.args` | yes | string value のみ |
| `build.target` | yes | multi-stage build target |
| `build.cacheFrom` | partial | Docker API で扱える形式 |
| `dockerComposeFile` | yes | Compose mode。string または array。`devcontainer.json` からの相対 path |
| `service` | yes | Compose mode の primary service |
| `runServices` | yes | Compose mode の started services。未指定時は全 services。primary service は常に含める |
| `features` | yes | OCI/local Feature |
| `overrideFeatureInstallOrder` | yes | Feature install order に反映 |
| `overrideCommand` | yes | image/Dockerfile mode の既定は true。Compose mode の既定は false |
| `mounts` | partial | bind/volume 対応。tmpfs は parse するが v0.1 では error |
| `workspaceMount` | yes | image/Dockerfile mode |
| `workspaceFolder` | yes | shell/lifecycle の working directory |
| `containerEnv` | yes | container create 時に適用 |
| `remoteEnv` | yes | exec/lifecycle/shell に適用 |
| `remoteUser` | yes | shell/lifecycle user |
| `containerUser` | yes | container process user |
| `updateRemoteUserUID` | yes | Linux host で既定 true |
| `userEnvProbe` | yes | `none`, `loginShell`, `interactiveShell`, `loginInteractiveShell` |
| `forwardPorts` | yes | decune forwarder |
| `portsAttributes` | partial | `label`, `onAutoForward`, `requireLocalPort`。`protocol`, `elevateIfNeeded` は warning して無視 |
| `otherPortsAttributes` | partial | automatic forwarding の既定。unsupported fields は warning |
| `appPort` | yes | Docker publish |
| `runArgs` | partial | allowlist の Docker run option のみ |
| `init` | yes | Docker HostConfig.Init |
| `privileged` | yes | Docker HostConfig.Privileged |
| `capAdd` | yes | Docker HostConfig.CapAdd |
| `securityOpt` | yes | Docker HostConfig.SecurityOpt |
| lifecycle commands | yes | Feature metadata 由来 command は user command より前に実行 |
| `waitFor` | partial | parse するが attached `up` は `postAttachCommand` まで同期実行 |
| `name` | ignored | runtime behavior には使わない |
| `shutdownAction` | partial | Compose mode の既定は `stopCompose`。`none`, `stopContainer`, `stopCompose` に対応 |
| `hostRequirements` | ignored | warning |
| `customizations` | ignored | preserve するが実行しない |

### Docker Compose mode

Compose mode は `devcontainer.json` に `dockerComposeFile` と `service` を指定した構成である。`service` が指す service を primary service と呼び、shell、lifecycle、Features、credential forwarding、port forwarding、UID/GID sync の対象にする。

`runServices` は started services を指定する。未指定時は Docker Compose の既定に従い全 services を起動対象にする。primary service は接続対象なので、`runServices` に含まれていない場合も started services に含める。

Compose project 名は decune が workspace identity から生成し、Docker Compose v2 CLI へ `--project-name` で明示する。ユーザー Compose file の top-level `name:` や directory basename には依存しない。

Dev Container metadata から primary service に反映する container-level 設定は generated override として runtime directory に作成し、ユーザー指定 Compose file の最後の `-f` として Docker Compose v2 CLI に渡す。generated override には secret value、token value、env 展開済みの raw config を保存しない。

Compose mode では `overrideCommand` の既定値を `false` とし、ユーザー Compose service の command を保持する。明示的に `overrideCommand = true` が指定された場合だけ、primary service に keepalive command を適用する。

Compose mode では `shutdownAction` の既定値を `stopCompose` とする。明示値は `none`、`stopContainer`、`stopCompose` を扱う。`down` / `clean` は Compose project 単位の停止・削除を行う。

### JSONC

`devcontainer.json` は JSON with Comments として扱う。コメント除去を正規表現で実装しない。trailing comma は JSONC として受け付ける。

### `runArgs` allowlist

v0.1 で受け付ける `runArgs` は以下のみ。

- `--init`
- `--privileged`
- `--cap-add <CAP>`
- `--security-opt <OPT>`
- `--add-host <HOST:IP>`
- `--dns <IP>`
- `--dns-search <DOMAIN>`

上記以外は unsupported error とする。`--publish` / `-p` は `appPort` または decune forwarding、`--mount` / `--volume` は `mounts`、`--user` は `containerUser`、環境変数は `containerEnv` を使う。

### `workspaceMount` / `workspaceFolder`

`workspaceMount` を明示する場合は `workspaceFolder` も明示必須とする。`workspaceFolder` は workspace mount target 配下でなければならない。`workspaceMount` 未指定時は `/workspaces/<localWorkspaceFolderBasename>` を bind mount target とし、`workspaceFolder` 未指定時はその target を working directory とする。

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
3. global decune config
4. `devcontainer.json`
5. project decune config
6. CLI flags

`--config <PATH>` は devcontainer metadata file を選択するだけであり、decune TOML overlay の追加指定ではない。

### merge rule

- scalar: 後勝ち。
- map: key ごとに merge。同一 key は後勝ち。
- decune TOML の array: 原則 append。ただし identity を持つ要素は置換。
- feature identity: canonical Feature ID と concrete ref。同一 concrete ref は option を merge する。`enabled = false` は canonical Feature ID 単位で無効化する。
- mount identity: `target`。
- dotfile identity: `target`。
- port identity: `protocol + container + host_ip`。
- hook identity: identity なし。順序を保って append。

### 設定例

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

### top-level

- `version`: 必須。v0.1 では `1` のみ。
- `shell`: 任意。`decune up` で attach する shell path または command 名。
- 未知の key は error。

### `[features]`

TOML の table key に Feature ref を quote して指定する。

```toml
[features."ghcr.io/devcontainers/features/go:1"]
version = "1.23"
enabled = true
```

- `enabled = false` で global/image metadata 由来 Feature を project 側から無効化できる。
- `enabled` は decune の予約 key であり、Feature option としては渡さない。
- それ以外の key は Feature option として扱う。

### `[[dotfiles]]`

dotfiles は host path を remote home に直接 bind mount しない。`/opt/decune/dotfiles/<target>` に mount し、container setup 時に remote user の home へ symlink を作る。

- `source`: host path。global config では `~` または absolute path。project config の relative path は workspace root 相対。
- `target`: remote home からの相対 path。absolute path は禁止。
- `enabled`: 既定 true。false の場合は同一 target を無効化。
- `read_only`: 既定 true。
- `resolve_symlink`: 既定 true。
- `on_conflict`: `fail`, `replace-symlink`, `backup`。既定 `fail`。

### `[[mounts]]`

任意の追加 mount。

- `type`: `bind`, `volume`, `tmpfs`。v0.1 では `bind` と `volume` に対応し、`tmpfs` は error。
- `source`: `bind` では必須。`volume` では volume 名。
- `target`: container absolute path。`/opt/decune` と `/run/decune` 配下、および workspace mount target と同一 target は禁止。
- `enabled`: 既定 true。false の場合は同一 target を無効化。
- `read_only`: 既定 false。
- `resolve_symlink`: bind source にのみ適用。既定 true。
- `create`: `false`, `"directory"`。既定 false。file の自動作成は行わない。

### `[[ports]]`

manual forwarding 設定。Docker publish ではない。

- `container`: container 側 port。必須。
- `host`: host 側 port。省略時は `container` と同じ番号を試し、占有済みなら空き port を探索する。
- `host_ip`: 既定 `127.0.0.1`。`0.0.0.0` は明示された場合のみ許可。
- `protocol`: v0.1 は `tcp` のみ。
- `enabled`: 既定 true。
- `require_local`: true の場合、host port が占有済みなら別 port に fallback せず失敗。
- `label`: 表示用。

### `[ports.auto]`

- `enabled`: 既定 true。
- `min`: 既定 1024。
- `max`: 既定 32768。
- `ignore`: automatic forwarding から除外する port。
- `on_auto_forward`: `notify`, `silent`, `ignore`。browser/preview 系は CLI では `notify` 相当。

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
- `https`: `off`, `host-helper`。既定 `host-helper`。
- `ssh_agent`: `off`, `auto`, `required`。既定 `auto`。

`host-helper` は container 内に `git-credential-decune` を配置し、host daemon 経由で host の `git credential fill/approve/reject` を呼ぶ。helper は container OS/arch 用 artifact であり、host の `decune` binary をそのまま bind mount しない。

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

Token value は argv、image layer、Docker label、container env、state、config hash に入れない。ただし container 内プロセスは token file に到達できるため、untrusted repository では `[credentials.github].enabled = false` を推奨する。

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

`${remoteUserHome}` は `/home/<user>` と推測せず、container/image 内の passwd database から解決する。`containerEnv` 自体の中で `${containerEnv:...}` を使う構成は v0.1 では error とする。

host bind source の処理順:

1. `~` を展開。
2. `${...}` を展開。
3. relative path を基準 directory から absolute path にする。
4. `create = "directory"` なら directory を作成。
5. `resolve_symlink = true` なら canonicalize。
6. 存在しない path は `create` が指定されていない限り error。

## Docker resource と state

workspace id:

```text
hex(sha256(canonical_workspace_path))[0..12]
```

Docker resource name には workspace basename をそのまま使わず、ASCII safe slug と workspace id を組み合わせる。

- container: `decune-<safe_workspace_slug>-<workspace_id>`
- image: `decune/<safe_workspace_slug>-<workspace_id>:<config_hash>`
- state directory: `$XDG_STATE_HOME/decune/<workspace_id>` または `~/.local/state/decune/<workspace_id>`
- runtime directory: `$XDG_RUNTIME_DIR/decune/<workspace_id>` または `/tmp/decune-<uid>/<workspace_id>`

主な Docker label:

- `decune.managed=true`
- `decune.workspace=<canonical_workspace_path>`
- `decune.workspace_id=<workspace_id>`
- `decune.config_hash=<hash>`
- `decune.version=<version>`
- `devcontainer.local_folder=<canonical_workspace_path>`
- `devcontainer.config_file=<path>`

既存 container の再利用は `decune.managed=true` と `decune.workspace_id` が一致するものに限る。他ツールの container は拾わない。

config hash には、resolved metadata/config、Feature lock、relevant CLI flags、Dockerfile 内容、effective ignore file、build context digest、entrypoint plan、Linux host の UID/GID sync input を含める。manual/automatic forwarding の現在値、credential token value、SSH agent socket path、GitHub token file path は含めない。

state file は `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` に保存する。write は atomic に行う。Docker label と state が矛盾する場合、container identity と config hash は Docker label を正とする。lifecycle 完了 flag は state に記録し、creation lifecycle の二重実行を避ける。

## Build と Features

image-based:

1. base image を pull する。`--pull` 指定時は常に pull を試す。
2. Feature があれば Feature 適用済み image を build する。
3. Linux host で UID/GID sync が必要なら sync layer を build する。
4. collected entrypoint があれば generated entrypoint shim layer を build する。
5. Feature、UID/GID sync、entrypoint shim が不要なら base image をそのまま create に使う。

Dockerfile-based:

1. `build.context` と `build.dockerfile` を `devcontainer.json` 相対で解決する。
2. Dockerfile-specific ignore file `<Dockerfile>.dockerignore` があれば context root の `.dockerignore` より優先する。
3. bollard build API へ tar context を渡す。
4. Dockerfile build 結果 image に Feature を重ねる。
5. 必要なら UID/GID sync layer と entrypoint shim layer を重ねる。

v0.1 では Dockerfile が build context 外にある構成を unsupported error とする。Dockerfile-based final image の `devcontainer.metadata` label は config hash と final image tag 決定の循環を避けるため merge せず、検出時は warning に留める。

Feature:

- OCI registry ref と local `./` ref に対応する。
- direct HTTPS tgz Feature は v0.1 では未対応。
- registry auth は Docker CLI 互換で `credHelpers`、`credsStore`、`auths` の順に source を選ぶ。選択 source が失敗しても別 source に fallback しない。
- manifest body と layer blob は sha256 digest を検証する。
- local Feature path は `devcontainer.json` directory からの相対 `./` path に限定し、absolute path と path escape を拒否する。
- local Feature directory basename と `devcontainer-feature.json` の `id` は一致必須。
- `devcontainer-feature.json` と `install.sh` は必須。
- OCI Feature は `<workspace>/.decune/features.lock.toml` に digest lock を記録する。
- `rebuild --update-features` は lock より再解決を優先する。
- Feature metadata は required field `id`, `version`, `name` を要求する。
- Feature option は Features 仕様に従って env key に変換し、default option も export する。env key collision は error。

## Container create/start と user

workspace mount 未指定時は `/workspaces/<localWorkspaceFolderBasename>` へ bind mount する。

user 解決:

- effective container user: `containerUser`、image/Feature metadata `containerUser`、Docker image config `User`、`root`。
- effective remote user: `remoteUser`、image/Feature metadata `remoteUser`、effective container user。

存在しない effective remote user は root fallback せず configuration error とする。numeric UID/GID は passwd entry がなくても runtime identity として扱えるが、home directory が必要な処理では error または warning skip になる。

`updateRemoteUserUID` は Linux host で既定 true。remote user が明示されていれば remote user、なければ `containerUser` が明示されている場合に container user を sync target とする。非 Linux host、root target、`updateRemoteUserUID = false`、passwd entry がない numeric target は no-op または warning skip とする。

## Lifecycle と shell attach

Dev Container lifecycle は以下の順で扱う。

1. `initializeCommand`（host）
2. `onCreateCommand`
3. `updateContentCommand`
4. `postCreateCommand`
5. `postStartCommand`
6. `postAttachCommand`

decune hook は各 lifecycle stage の前後に実行する。Feature metadata 由来 lifecycle command は Feature install order 順に収集し、user の `devcontainer.json` 由来 command より先に実行する。

lifecycle command が失敗した場合、対応する after hook と後続処理は実行しない。creation lifecycle の成功済み stage は state に記録し、次回 reuse 時に二重実行しない。

non-detach `up` / `rebuild` は lifecycle 後に remote user shell を TTY attach し、shell exit code を CLI exit code として返す。`--detach` では attach lifecycle、forwarding listener、`postAttachCommand`、shell attach を実行しない。

## Git/GitHub 認証

### Git HTTPS

`[credentials.git].https = "host-helper"` の場合、container 内に `git-credential-decune` を配置し、Git credential helper として設定する。helper は host daemon に versioned JSON request を送り、host の `git credential fill/approve/reject` を実行する。

### SSH agent

`ssh_agent = "auto"` では host の `SSH_AUTH_SOCK` が Unix socket の場合のみ forwarding を設定する。container env の `SSH_AUTH_SOCK` は `/run/decune/ssh-agent.sock`。`ssh_agent = "required"` で socket が利用できない場合は error。

### GitHub CLI

host の `gh auth token` が成功した場合、token を runtime directory に mode 0600 の file として作り、container には `/run/decune/secrets/github-token` として read-only mount する。`GH_CONFIG_DIR=/run/decune/gh` は writable ephemeral directory とする。token file は `up` 終了時に scrub し、`down` / `clean` で削除する。

## Port forwarding

`forwardPorts`、decune `[[ports]]`、CLI `-p` は forwarding であり Docker publish ではない。host 側 listen address の既定は `127.0.0.1`。container 内で `127.0.0.1:<container port>` にだけ listen している process にも届くよう、container-side `decune-forward-agent` 経由で proxy する。

`appPort` は Docker publish であり container create 時に決まる。host IP が指定されない場合、Docker の既定で全 interface に公開される可能性があるため warning 対象とする。

manual forwarding source priority:

1. CLI `-p`
2. project decune `[[ports]]`
3. devcontainer `forwardPorts`
4. global decune `[[ports]]`

host port が占有済みの場合、`require_local = true` なら失敗し、false なら昇順で空き port を探索する。

automatic forwarding は container agent が `/proc/net/tcp` と `/proc/net/tcp6` を読み、LISTEN port を検出する。既定 scan interval は 2 秒、initial delay は 3 秒。manual forwarding 済み、Docker publish 済み、ignore list、`portsAttributes.onAutoForward = "ignore"` は除外する。

## Host daemon と security boundary

host daemon は `decune up` の子タスクとして起動し、`up` 終了時に停止する。常駐 system daemon ではない。

責務:

- Git credential helper request の処理。
- GitHub token file の一時管理。
- port forwarding runtime の socket 基盤。

禁止:

- container から任意 host command を実行する API を提供しない。
- Docker socket を container に暗黙 mount しない。

runtime directory は 0700、socket は 0600 を基本とする。permission 調整時も host daemon は Unix socket peer UID を検証する。

Security note:

- `decune up` は Dockerfile、local/OCI Feature の `install.sh`、Feature/lifecycle command、hook、`userEnvProbe` 対象 shell startup file を実行し得る。
- devcontainer metadata は bind mount、`privileged`、`capAdd`、`securityOpt`、`appPort` publish、SSH agent forwarding、Git/GitHub credential forwarding により host や secret への強い到達性を container へ与え得る。
- GitHub token forwarding を有効にすると、container 内 process は token file にアクセスできる。
- untrusted repository では `.devcontainer/` と local Feature を確認し、必要に応じて `[credentials.git].enabled = false` と `[credentials.github].enabled = false` を設定する。

## 検証方針

通常の formatting / lint:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets -- -D warnings
```

Docker integration test を含む full test:

```sh
cargo run --locked -p xtask -- build-container-tools --out assets/container-tools --locked
cargo run --locked -p xtask -- check-container-tools --dir assets/container-tools
DECUNE_CONTAINER_TOOLS_BUNDLE=required cargo test --workspace --all-features --no-fail-fast
```

Docker API 互換 daemon に接続できない環境では full test は失敗として扱う。純粋ロジックだけ確認する場合は、対象 package/module/test 名で filter して実行する。

主な integration test 対象:

- image-based up/down/clean/rebuild。
- Dockerfile build と `--no-cache`。
- Dockerfile-specific ignore file の context hash / tar context 反映。
- read-only bind mount と symlink source mount。
- dotfiles symlink setup。
- lifecycle failure と lifecycle 二重実行防止。
- `overrideCommand`、Feature entrypoint shim。
- manual / automatic forwarding。
- `appPort` warning と unsupported port attributes warning。
- UID/GID sync。
- Feature metadata required fields、Feature option env/default、local Feature constraints。
- Docker resource name sanitization。
- non-TTY `clean` without `--force` failure。
- state repair と secret leak regression。
