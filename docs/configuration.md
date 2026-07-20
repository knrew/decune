# decune config ガイド

この文書は、decune config の使い方と挙動の説明をまとめた利用者向けガイドです。スキーマ、既定値、マージルールの正は [specification.md 5 章](specification.md#5-decune-config)、変数展開は [6 章](specification.md#6-変数展開とパス解決)です。ポート関連の設定(`[[ports]]`、`[ports.auto]`、`[compose.published_ports]`)は [ports.md](ports.md)、`[compose.clone_isolation]` は [clone-isolation.md](clone-isolation.md) を参照してください。

## 設定ファイルの場所と重ね合わせ

- global decune config: `$XDG_CONFIG_HOME/decune/config.toml` または `~/.config/decune/config.toml`
- project decune config: `<workspace>/.decune/config.toml`

decune config は `devcontainer.json` に対するオーバーレイであり、ベースイメージ / ビルド / Compose 定義の置き換えには使えません。個人環境の好み(dotfiles、シェル、認証情報の方針など)は global decune config に、リポジトリで共有したい設定(Feature、マウント、clone isolation など)は project decune config に置きます。project decune config は Git 管理してかまいませんが、秘密情報は設定ファイルに直接書かないでください。

設定は複数のレイヤーを重ねて合成され、基本は後勝ちです。おおまかには global decune config → `devcontainer.json` → project decune config → CLI オプションの順で後が優先されます。イメージ / Feature のメタデータを含む完全な順序は [specification.md 5.2 節](specification.md#52-マージ順序)、フィールドごとのマージルールは [5.3 節](specification.md#53-マージルール)を参照してください。

global decune config を適用したくない場合は、project decune config に `use_global_config = false` を書くか、`decune up --no-global-config` を使います。CLI オプションは一時的な強制無効化で、project decune config で再有効化できません。

## トップレベル設定

```toml
version = 1
shell = "/bin/zsh"
```

- `version`: 必須で、`1` だけが有効です。未知のキーはエラーになります。
- `shell`: `decune up` で接続するシェルのパスまたはコマンド名です。
- `use_global_config`: project decune config で false にすると global decune config を適用しません。

## `[features]`

Dev Container Feature を追加し、オプションを指定します。テーブルキーに Feature の参照を引用符で囲んで書きます。

```toml
[features."ghcr.io/devcontainers/features/go:1"]
version = "1.23"
```

- `enabled = false` を指定すると、global decune config やイメージ / Feature メタデータ由来の Feature を project decune config から無効化できます。`enabled` は decune の予約キーで、Feature のオプションとしては渡されません。
- OCI Feature の解決結果は `<workspace>/.decune/features.lock.toml` に digest lock として記録されます。レジストリ / タグを再解決して lock より新しい版を取り込むには `decune rebuild --update-features` を実行します。

スキーマは [specification.md 5.6 節](specification.md#56-features)、Feature の解決とビルドの契約は [7.1 節](specification.md#71-ビルドと-feature)を参照してください。

## `[[dotfiles]]`

ホストの dotfiles をコンテナのリモートユーザーのホームディレクトリから使えるようにします。

```toml
[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
```

- `source` はホスト側パス、`target` はリモートユーザーのホームディレクトリからの相対パスです。
- dotfiles はリモートユーザーのホームディレクトリへ直接 bind mount されず、`/opt/decune/dotfiles/<target>` にマウントしてホームディレクトリから symlink されます。ホームディレクトリ側には設定した dotfiles のエントリだけが現れます。
- `read_only` の既定は true です。書き込みたいエントリだけ `read_only = false` にします。
- `on_conflict` はリモートユーザーのホームディレクトリ側に既存エントリがある場合の扱いです(`fail` / `replace-symlink` / `backup`。既定は `fail`)。
- `resolve_symlink`(既定 true)は source の symlink を正規化します。`~/.config/nvim` 自体が symlink でも実体をマウントできます。

知っておくとよい挙動:

- 配下に symlink を含むディレクトリの source を直接の bind mount で表現できない場合は skeleton による代替になります。このとき source 由来ファイルの親ディレクトリを `/opt/decune/dotfile-backings/<n>` に bind mount するため、同じ親ディレクトリの兄弟ファイルがコンテナからそのパス経由で見える場合があります。
- `read_only = false` の skeleton 経由のパスにコンテナ内から新規作成したファイルは、元の source ではなく decune の状態領域(skeleton)に保存され、以後も保持されます。
- ホスト側で通常ファイルや解決済みの symlink 先ファイルを atomic rename で置き換えた場合(多くのエディタの保存方式)は、起動中のコンテナからも新しい内容が見えます。source の symlink のパス自体を通常ファイルに置き換えた場合は自動反映されないため、コンテナを再作成してください。

詳細な契約は [specification.md 5.7 節](specification.md#57-dotfiles)を参照してください。

## `[[mounts]]`

ワークスペース以外のホスト側ディレクトリやボリュームを追加でマウントします。

```toml
[[mounts]]
source = "~/work"
target = "/workspaces/work"
type = "bind"
```

- `type` は `bind` と `volume` に対応します(`tmpfs` はエラー)。ホストのファイルを共有するなら `bind`、ホスト側パスに依存しないデータ領域が欲しいなら名前付きの `volume` を使います。
- `source` は `bind` ではホスト側パス、`volume` ではボリューム名です。
- `create = "directory"` にすると、存在しない bind の source ディレクトリを作成してからマウントします。ファイルの自動作成はありません。
- `resolve_symlink`(既定 true)は bind の source の symlink を正規化します。
- `target` に `/opt/decune` と `/run/decune` の配下、およびワークスペースのマウント先と同一のパスは使えません。decune の内部パスとして予約されています。
- Docker Compose-based configuration では primary service(`service` で指定した Compose サービス)に decune-generated Compose override として追加されます。

スキーマは [specification.md 5.8 節](specification.md#58-mounts)を参照してください。

## `[credentials.git]`

ホストの Git 認証情報をコンテナから使えるようにします。

```toml
[credentials.git]
https = "host-helper"
ssh_agent = "auto"
```

- `https`(既定 `host-helper`)はコンテナからの Git credential 要求をホストの credential helper へ転送します。`host-helper-read-only` は認証情報の読み出し(`get`)だけを転送し、`store` / `erase` はホストに伝えず成功として扱います。`off` で無効にします。
- `ssh_agent`(既定 `auto`)はホストの `SSH_AUTH_SOCK` が使える場合だけ SSH agent forwarding を設定します。必須にするなら `required`、不要なら `off` にします。SSH agent forwarding は HTTPS のヘルパーとは別経路なので、`https = "host-helper-read-only"` にしても無効にはなりません。
- `copy_user`(既定 true)はホストの `git config --global` の `user.name` / `user.email` をコンテナのリモートユーザーに設定します。`copy_global_config = true` にすると `~/.gitconfig` 全体をコンテナにコピーします。

信頼していないリポジトリ向けの推奨構成は [usage.md の安全な使い方](usage.md#安全な使い方)を参照してください。スキーマは [specification.md 5.14 節](specification.md#514-credentialsgit)、転送経路と到達性の境界は [12.3 節](specification.md#123-credential-forwarding-と到達性)を参照してください。

## `[credentials.github]`

ホストの GitHub CLI トークンをコンテナから使えるようにします。

```toml
[credentials.github]
mode = "gh-token-file"
```

- `mode = "gh-token-file"`(既定)はホストの `gh auth token` から一時トークンファイルを作り、コンテナに `/run/decune/secrets/github-token` として read-only でマウントします。
- `install_feature_if_missing`(既定 true)は、ホストのトークンが取得できコンテナに `gh` がない場合に GitHub CLI Feature を追加します。
- トークンの値は Docker ラベル、コンテナの環境変数、状態、reuse hash、decune-generated Compose override には保存されませんが、コンテナ内プロセスはトークンファイルに到達できます。信頼していないリポジトリでは `enabled = false` にしてください([usage.md の安全な使い方](usage.md#安全な使い方))。

スキーマは [specification.md 5.15 節](specification.md#515-credentialsgithub)を参照してください。

## `[container.cli]`

コンテナ内からの read-only のクエリ(`decune status` / `decune ports`)を許可するかを設定します。使い方は [usage.md](usage.md#コンテナ内の-decune) を参照してください。

```toml
[container.cli]
enabled = true
```

- 有効(既定)の場合、`decune up` は primary container に decune container CLI を `/run/decune/decune` として配置し、最初の lifecycle command より前に `/usr/local/bin/decune` の symlink を準備します。
- 通常の後勝ち設定なので、global decune config の `false` は project decune config の `true` で再有効化できます。リポジトリから解除できないセキュリティ上のオプトアウトではありません。
- この設定は reuse hash に含まれないため、変更してもコンテナ / Compose プロジェクトの再作成は不要です。
- `/usr/local/bin/decune` に既存のファイル、ディレクトリ、別の symlink がある場合や、ルートファイルシステムが read-only の場合、decune は既存の配置先を変更せず警告を出して `up` を継続します。その場合はコンテナ内で `/run/decune/decune` を直接実行してください。

スキーマは [specification.md 5.13 節](specification.md#513-containercli)、artifact と symlink の扱いは [12.6 節](specification.md#126-decune-container-cli-artifact-と-symlink)を参照してください。

## `[[hooks.*]]`

lifecycle stage の前後に実行する decune hook を定義します。フック名は `before_` / `after_` + lifecycle stage 名です(`before_post_create`、`after_initialize` など。一覧は [specification.md 5.16 節](specification.md#516-hooks))。

```toml
[[hooks.before_post_create]]
command = "scripts/setup.sh"
where = "container"
user = "remote"
shell = true
```

- `command` は文字列または文字列の配列です。文字列は既定でシェル(`/bin/sh -lc`)実行、配列は既定で直接実行です(`shell` で明示できます)。
- `where` は `host` / `container` です。`initialize` 系のフックはホストのみです。
- `user` はコンテナ側フックの実行ユーザーです(`remote` / `root` / `<name>`。既定 `remote`)。
- `workdir` 省略時は、ホスト側フックは workspace root、コンテナ側フックは `workspaceFolder` で実行されます。
- フックは識別子を持たず、設定レイヤーをまたいで順序を保って追加されます。後のレイヤーで置換・削除はできません。

lifecycle の順序と decune hook の実行タイミングは [specification.md 7.3 節](specification.md#73-lifecycle-とシェル接続)を参照してください。

## ポートと clone isolation の設定

- `[[ports]]`(manual port forwarding)、`[ports.auto]`(automatic port forwarding)、`[compose.published_ports]`(published port の mapping / relocation)は [ports.md](ports.md) を参照してください。
- `[compose.clone_isolation]`(複数クローンの同時利用)は [clone-isolation.md](clone-isolation.md) を参照してください。
