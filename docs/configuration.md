# decune config ガイド

この文書は、decune config の使い方と挙動の説明をまとめた利用者向けガイドです。スキーマ、既定値、merge rule の正は [specification.md 5 章](specification.md#5-decune-config)、変数展開は [6 章](specification.md#6-変数展開と-path-解決)です。ポート関連の設定(`[[ports]]`、`[ports.auto]`、`[compose.published_ports]`)は [ports.md](ports.md)、`[compose.clone_isolation]` は [clone-isolation.md](clone-isolation.md) を参照してください。

## 設定ファイルの場所と重ね合わせ

- global config: `$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`
- project config: `<workspace>/.decune/config.toml`

decune config は `devcontainer.json` に対する overlay であり、base image / build / Compose 定義の置き換えには使えません。個人環境の好み(dotfiles、shell、credentials の方針など)は global config に、リポジトリで共有したい設定(Features、mounts、clone isolation など)は project config に置きます。project config は Git 管理してかまいませんが、秘密情報は設定 file に直接書かないでください。

設定は複数の layer を重ねて合成され、基本は後勝ちです。おおまかには global config → `devcontainer.json` → project config → CLI options の順で後が優先されます。image/Feature metadata を含む完全な順序は [specification.md 5.2 節](specification.md#52-merge-順序)、field ごとの merge rule は [5.3 節](specification.md#53-merge-rule)を参照してください。

global config を適用したくない場合は、project config に `use_global_config = false` を書くか、`decune up --no-global-config` を使います。CLI option は一時的な強制無効化で、project config で再有効化できません。

## トップレベル設定

```toml
version = 1
shell = "/bin/zsh"
```

- `version`: 必須で、`1` だけが有効です。未知の key は error になります。
- `shell`: `decune up` で attach する shell の path または command 名です。
- `use_global_config`: project config で false にすると global config を適用しません。

## `[features]`

Dev Container Feature を追加し、option を指定します。table key に Feature ref を quote して書きます。

```toml
[features."ghcr.io/devcontainers/features/go:1"]
version = "1.23"
```

- `enabled = false` を指定すると、global config や image/Feature metadata 由来の Feature を project 側から無効化できます。`enabled` は decune の予約 key で、Feature option としては渡されません。
- OCI Feature の解決結果は `<workspace>/.decune/features.lock.toml` に digest lock として記録されます。registry/tag を再解決して lock より新しい版を取り込むには `decune rebuild --update-features` を実行します。

スキーマは [specification.md 5.6 節](specification.md#56-features)、Feature の解決と build の契約は [7.1 節](specification.md#71-build-と-features)を参照してください。

## `[[dotfiles]]`

host の dotfiles を container の remote home から使えるようにします。

```toml
[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
```

- `source` は host path、`target` は remote home からの相対 path です。
- dotfiles は remote home へ直接 bind mount されず、`/opt/decune/dotfiles/<target>` に mount して remote home から symlink されます。remote home 側には設定した dotfile entry だけが現れます。
- `read_only` の既定は true です。書き込みたい entry だけ `read_only = false` にします。
- `on_conflict` は remote home 側に既存 entry がある場合の扱いです(`fail` / `replace-symlink` / `backup`。既定は `fail`)。
- `resolve_symlink`(既定 true)は source の symlink を canonicalize します。`~/.config/nvim` 自体が symlink でも実体を mount できます。

知っておくとよい挙動:

- 配下に symlink を含む directory source を直接の bind mount で表現できない場合は skeleton fallback になります。このとき source 由来 file の親 directory を `/opt/decune/dotfile-backings/<n>` に bind mount するため、同じ親 directory の sibling file が container からその path 経由で見える場合があります。
- `read_only = false` の skeleton 経由 path に container 内から新規作成した file は、元の source ではなく decune の state 領域(skeleton)に保存され、以後も保持されます。
- host 側で通常 file や解決済み symlink 先の file を atomic rename で置き換えた場合(多くの editor の保存方式)は、起動中の container からも新しい内容が見えます。source の symlink path 自体を regular file に置き換えた場合は自動反映されないため、container を recreate してください。

詳細な契約は [specification.md 5.7 節](specification.md#57-dotfiles)を参照してください。

## `[[mounts]]`

workspace 以外の host directory や volume を追加で mount します。

```toml
[[mounts]]
source = "~/work"
target = "/workspaces/work"
type = "bind"
```

- `type` は `bind` と `volume` に対応します(`tmpfs` は error)。host のファイルを共有するなら `bind`、host path に依存しないデータ領域が欲しいなら named `volume` を使います。
- `source` は `bind` では host path、`volume` では volume 名です。
- `create = "directory"` にすると、存在しない bind source directory を作成してから mount します。file の自動作成はありません。
- `resolve_symlink`(既定 true)は bind source の symlink を canonicalize します。
- `target` に `/opt/decune` と `/run/decune` の配下、および workspace mount target と同一の path は使えません。decune の internal path として予約されています。
- Docker Compose-based configuration では primary service(`service` で指定した Compose service)に generated override として追加されます。

スキーマは [specification.md 5.8 節](specification.md#58-mounts)を参照してください。

## `[credentials.git]`

host の Git 認証情報を container から使えるようにします。

```toml
[credentials.git]
https = "host-helper"
ssh_agent = "auto"
```

- `https`(既定 `host-helper`)は container からの Git credential 要求を host の credential helper へ転送します。`host-helper-read-only` は credential の読み出し(`get`)だけを転送し、`store` / `erase` は host に伝えず success no-op にします。`off` で無効にします。
- `ssh_agent`(既定 `auto`)は host の `SSH_AUTH_SOCK` が使える場合だけ SSH agent forwarding を設定します。必須にするなら `required`、不要なら `off` にします。SSH agent forwarding は HTTPS helper とは別経路なので、`https = "host-helper-read-only"` にしても無効にはなりません。
- `copy_user`(既定 true)は host の `git config --global` の `user.name` / `user.email` を container の remote user に設定します。`copy_global_config = true` にすると `~/.gitconfig` 全体を container にコピーします。

信頼していないリポジトリ向けの推奨構成は [usage.md の安全な使い方](usage.md#安全な使い方)を参照してください。スキーマは [specification.md 5.14 節](specification.md#514-credentialsgit)、転送経路と到達性の境界は [12.3 節](specification.md#123-credential-転送と到達性)を参照してください。

## `[credentials.github]`

host の GitHub CLI token を container から使えるようにします。

```toml
[credentials.github]
mode = "gh-token-file"
```

- `mode = "gh-token-file"`(既定)は host の `gh auth token` から一時 token file を作り、container に `/run/decune/secrets/github-token` として read-only mount します。
- `install_feature_if_missing`(既定 true)は、host token が取得でき container に `gh` がない場合に GitHub CLI Feature を追加します。
- token value は Docker label、container env、state、config hash、generated Compose override には保存されませんが、container 内プロセスは token file に到達できます。信頼していないリポジトリでは `enabled = false` にしてください([usage.md の安全な使い方](usage.md#安全な使い方))。

スキーマは [specification.md 5.15 節](specification.md#515-credentialsgithub)を参照してください。

## `[container.cli]`

container 内からの read-only query(`decune status` / `decune ports`)を許可するかを設定します。使い方は [usage.md](usage.md#container-内の-decune) を参照してください。

```toml
[container.cli]
enabled = true
```

- 有効(既定)の場合、`decune up` は primary container に container-side CLI を `/run/decune/decune` として配置し、最初の lifecycle command より前に `/usr/local/bin/decune` symlink を準備します。
- 通常の後勝ち設定なので、global config の `false` は project config の `true` で再有効化できます。repository から解除不能な security opt-out ではありません。
- この設定は config hash に含まれないため、変更しても container / Compose project の rebuild は不要です。
- `/usr/local/bin/decune` に既存の file、directory、別の symlink がある場合や、root filesystem が read-only の場合、decune は既存の destination を変更せず warning を出して `up` を継続します。その場合は container 内で `/run/decune/decune` を直接実行してください。

スキーマは [specification.md 5.13 節](specification.md#513-containercli)、artifact と symlink の扱いは [12.6 節](specification.md#126-container-cli-artifact-と-symlink)を参照してください。

## `[[hooks.*]]`

lifecycle stage の前後に decune 固有の command を実行します。hook 名は `before_` / `after_` + lifecycle stage 名です(`before_post_create`、`after_initialize` など。一覧は [specification.md 5.16 節](specification.md#516-hooks))。

```toml
[[hooks.before_post_create]]
command = "scripts/setup.sh"
where = "container"
user = "remote"
shell = true
```

- `command` は string または string array です。string は既定で shell(`/bin/sh -lc`)実行、array は既定で直接実行です(`shell` で明示できます)。
- `where` は `host` / `container` です。`initialize` 系の hook は host のみです。
- `user` は container hook の実行 user です(`remote` / `root` / `<name>`。既定 `remote`)。
- `workdir` 省略時は、host hook は workspace root、container hook は `workspaceFolder` で実行されます。
- hook は識別子を持たず、設定 layer をまたいで順序を保って追加されます。後の layer で置換・削除はできません。

lifecycle の順序と hook の実行タイミングは [specification.md 7.3 節](specification.md#73-lifecycle-とシェル接続)を参照してください。

## ポートと clone isolation の設定

- `[[ports]]`(manual port forwarding)、`[ports.auto]`(automatic port forwarding)、`[compose.published_ports]`(published port の mapping / relocation)は [ports.md](ports.md) を参照してください。
- `[compose.clone_isolation]`(複数 clone の同時利用)は [clone-isolation.md](clone-isolation.md) を参照してください。
