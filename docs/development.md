# decune の開発

この文書は、コントリビューター向けの環境構築、検証、リリース成果物の作成コマンド、ドキュメント執筆規約をまとめます。利用手順は [usage.md](usage.md)、公開挙動は [specification.md](specification.md)、内部実装の説明は [internals.md](internals.md)、maintainer 向けのリリース手順は [release.md](release.md) を参照してください。

## ソースからのローカルインストール

ソースコードからのインストールは、公式のローカルインストール手順です。Git credential forwarding と port forwarding に必要なコンテナ側ツールを build し、ホスト側バイナリに埋め込んでから `decune` をインストールします。

Rust toolchain は `Cargo.toml` の `rust-version` 以上の stable を使います。decune の MSRV は current stable 追従方針で、古い Rust minor version の長期サポートは対象外です。

Linux 用のコンテナ側ツールを build するため、以下の Rust target が必要です。

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
```

xtask のインストールコマンドを実行します。

```sh
cargo run --locked -p xtask -- install --locked
```

この command は `target/decune-xtask/container-tools-bundle` に `git-credential-decune`、`decune-forward-agent`、container-side `decune` の 3 tools を 2 platform 向けに build/check し、6 artifact の bundle を埋め込んだ host-side `decune` を `cargo install` します。container-side `decune` の Cargo binary target は `decune-container-cli`、bundle 内の artifact name は `decune` です。bundle の build/check だけを個別に確認する場合は、以下を実行します。

```sh
cargo run --locked -p xtask -- build-container-tools --locked
cargo run --locked -p xtask -- check-container-tools
```

コンテナ側ツールの bundle を埋め込まない build は公式インストール手順ではありません。軽いローカル確認だけならインストールせず、通常の Cargo コマンドを使ってください。bundle の埋め込みの仕組み、build 時の内部環境変数、開発用 override `DECUNE_CONTAINER_TOOLS_DIR` の説明は [internals.md](internals.md#6-container-tools-bundle-と-runtime-staging) にあります。

## 開発ビルドのバージョン表示

リリース直後の開発中は、次のリリース番号を先に固定しないため root `Cargo.toml` の `[workspace.package]` version は直近リリース版のままにします。`decune --version` は clean な release tag では `decune 0.1.0` のように表示し、tag 外や未コミット変更を含む build では `decune 0.1.0+g<commit>` または `decune 0.1.0+g<commit>.dirty` のように Git 由来の build metadata を付けます。

Git 情報を取得できない source build では `+source` suffix を付けます。この suffix は表示用であり、Docker label などの内部 metadata には Cargo が解決した package version を使います。公式配布 artifact の version 表示規則の正は [specification.md 11 章](specification.md#11-配布の契約)です。

## 標準検証

通常の変更では formatting と lint を確認します。markdownlint の対象は Git 管理下の共有 Markdown(README.md と docs/ 配下)です。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets
NPM_CONFIG_CACHE=/tmp/decune-npm-cache npx -y markdownlint-cli2@0.22.1 --config .markdownlint.yaml README.md docs/*.md
```

Shell script formatting は `.editorconfig` の shell 用設定を `shfmt` が読む形で管理します。

対象を絞った test は package/module/test filter を指定して実行します。

```sh
cargo test -p decune <module_or_test_name>
```

## テストの検証範囲

decune の test coverage は、少なくとも以下の挙動グループを含めます。公開挙動を変更する場合は、該当するグループの test も同じ変更で更新してください。

- image-based / Dockerfile-based / Docker Compose-based の `up` / `rebuild` / `down` / `remove`。
- Dockerfile build の入力、`.dockerignore` の扱い、`--no-cache`、`--pull`、未対応の Dockerfile/context 組み合わせ。
- Compose の `dockerComposeFile`、`service`、`runServices`、profiles、複数 file の merge、generated override の挙動、project cleanup の安全性。
- Feature 解決、lock の扱い、metadata merge、option env/default の扱い、local Feature の制約、UID/GID sync、entrypoint shim の挙動。
- dotfiles、mounts、lifecycle commands、hooks、shell attach、lifecycle の二重実行防止。
- manual/automatic port forwarding、published port の warning/error、sidecar 明示 forwarding、TCP-only の挙動。
- credential forwarding、token redaction、state repair、resource name の sanitization、secret leak regression coverage。
- container CLI の image/Dockerfile/Compose primary、command/stdio/exit matrix、UID/sidecar topology、attached/detached、enabled lifecycle、symlink fallback、forwarding 集約・daemon handoff、live workspace/host path 非参照。

## テスト fixture 管理

テストで使う長い shell script、TOML、JSON、YAML、Dockerfile は、原則として `tests/fixtures` 配下に置き、test 実行時に読み込みます。新規の test fixture に `include_str!` / `include_bytes!` は使いません。

配置は用途別に揃えます。

- `tests/fixtures` 配下の shell fixture の directory/file 名は `kebab-case` に揃える。
- Docker Compose integration 用 workspace fixture は `tests/fixtures/compose/` に置く。
- CLI harness 共通 fixture は `tests/fixtures/cli/harness/` に置く。
- 複数 test で使う fake `docker` / `gh` / `git` / `curl` などは `tests/fixtures/cli/fake-bin/` に置く。
- module 固有の長い fake command は `tests/fixtures/cli/<module>/<case>.sh` に置く。
- 複数 file で構成される CLI workspace fixture は `tests/fixtures/cli/workspaces/<module>/<case>/` に置く。

test から fixture を読む場合は `tests/support/support.rs` の helper を使います。fixture path と temporary workspace path は absolute path と `..` を拒否します。temporary workspace へ fixture を置く場合は `TempWorkspace::copy_fixture_dir`、`TempWorkspace::write_fixture_file`、`TempWorkspace::write_fixture_template`、`TempWorkspace::write_executable_fixture` を使ってください。

test が host 側に作る一時 file / directory は、原則として `tempfile::TempDir`、`tempfile::NamedTempFile`、またはそれらを所有する共通 helper で管理します。`std::env::temp_dir()` から直接 path を組み立て、test の先頭や末尾だけで手動 cleanup する構成にはしません。一時 path を command、workspace、非同期処理へ渡す場合は、その利用が終わるまで所有者を保持してください。通常は所有者の `Drop` に cleanup を任せ、cleanup 失敗自体を test failure として確認する必要がある場合だけ `TempDir::close()` などを使います。

複数 process が同じ inode を lock するための file など、process 間で安定した path が必要な場合は例外です。この場合は一時領域を test ごとに分離せず、直接 path が必要な理由と cleanup 方針を code に明記します。

CLI test で fake host command を `PATH` に置く場合は、module-local helper を増やさず `tests/cli/harness.rs` の `fake_command_path`、`fake_docker_path`、`fake_path_with_commands` などを使います。

Runtime 値が必要な fixture では `__HOST_PORT__` のような placeholder を置き、`write_fixture_template` で展開します。placeholder を指定したのに fixture 内に存在しない場合は test helper が失敗します。数行だけの異常系入力、assert の近くにある方が意図を読みやすい marker/config は inline のままでも構いません。

動的に `source` する必要がある箇所だけ、該当行の直前に `# shellcheck disable=SC1090` を付けます。Git 上の shell fixture は通常 `100644` のままにし、実行が必要な test では `TempWorkspace::write_executable_fixture` で temporary workspace 側に `0755` として書き出します。

## Docker を使う検証

Docker-backed test を含む全体検証には、Docker デーモンと Docker Compose v2 プラグインへ接続できる環境が必要です。

```sh
cargo run --locked -p xtask -- workspace-test
cargo run --locked -p xtask -- compose-integration
```

これらの command は container tools の build と `cargo test --verbose` の進捗を逐次出力します。test 本体の標準出力は Rust test harness の既定どおり capture され、失敗時に表示されます。

Compose integration test だけを実行する場合:

```sh
docker version
docker compose version
cargo run --locked -p xtask -- compose-integration
```

`compose_integration` の Docker-backed test は `#[ignore]` として定義します。通常の unit test では実行されず、`compose-integration` が Docker/Compose availability を確認したうえで ignored integration test を one test thread で実行します。

この経路は通常の Compose scenario に加え、実 bundle の container-side 3 tools × 2 platforms を build/check し、container CLI の image-based / Dockerfile-based / Docker Compose-based E2E を実行します。container CLI E2E は attached `up` process と別の `docker exec` process を使い、query、UID/sidecar topology、lifecycle、forwarding handoff、sanitized disclosure boundary を real Docker resource で確認します。

## リリース成果物

配布アーカイブを手元で作成します。正式リリースでは、tag push 後に GitHub Actions の `Release` workflow が同じ `xtask` を使って成果物を作成します。tag 作成から公開後確認までの手順は [release.md](release.md) を参照してください。

```sh
cargo run --locked -p xtask -- dist \
  --target x86_64-unknown-linux-musl \
  --version 0.1.0 \
  --locked
```

checksum と release manifest を生成します。

```sh
cargo run --locked -p xtask -- checksum --dist-dir target/dist --version 0.1.0
cargo run --locked -p xtask -- release-manifest --dist-dir target/dist --version 0.1.0
```

生成済みのバイナリ成果物は Git リポジトリに commit しません。

## ドキュメント構成と執筆規約

この節は、decune のドキュメント構成と責務分担の唯一の定義です。各文書の冒頭にある 1 行の責務宣言と相互リンクは、この節の要約です。

### 文書の責務

| 文書                                 | 責務                                                                                       |
| ------------------------------------ | ------------------------------------------------------------------------------------------ |
| [README.md](../README.md)            | 概要と最短導線(初見の利用者向け)                                                           |
| [usage.md](usage.md)                 | 利用者向けのインストール、日常操作、設定、応用機能のガイド                                 |
| [specification.md](specification.md) | 公開挙動、CLI 契約、設定スキーマ、セキュリティ境界、診断コード定義の正本(リファレンス兼務) |
| [internals.md](internals.md)         | 内部実装の説明(非規範の内部設計ノート)                                                     |
| development.md(この文書)             | 貢献者向けの環境構築、検証、ドキュメント執筆規約                                           |
| [release.md](release.md)             | maintainer 向けのリリース runbook                                                          |
| [glossary.md](glossary.md)           | 用語と表記基準                                                                             |

### 情報の置き場所

同じ情報は 1 つの文書だけを正とし、他の文書は要約とリンクに徹します。正と矛盾する記述を他の文書に持ち込まないでください。

- 公開挙動、CLI の契約、設定スキーマと既定値、セキュリティ境界、診断コードの発生条件の正は specification.md。
- 操作手順、設定・機能の使い方、診断コードへの対処、untrusted repository での推奨設定の正は usage.md。
- ソースからのインストール、検証コマンド、test coverage 要求、ドキュメント執筆規約の正はこの文書。
- 実装定数、内部型名、内部環境変数、runtime file レイアウトなど内部実装の説明は internals.md。非規範であることを明記し、挙動の約束は書かない。
- 配布物の契約(asset、検証手段、version 表示規則)の正は specification.md の配布章。リリース手順の正は release.md、開発ビルドの version 運用はこの文書。
- 用語と表記の正は glossary.md。本文で用語を追加・変更した場合は glossary.md も更新する。

### 執筆規約

- 公開挙動、CLI option、設定 key、セキュリティ境界を変更した場合は、同じ変更で specification.md と関連する利用者向けドキュメントも更新する。
- 仕様の規範記述は specification.md だけに書く。ガイドに書いてよい仕様記述は、その場面の理解に必要な要約 1〜2 文と specification.md へのリンクまでとする。
- README と usage は仕様を要約できるが、仕様と矛盾する内容を持たない。仕様、README、usage、実装、test が矛盾する場合は、暗黙に実装を正とせず、差分の意図を確認してから揃える。
- 実装作業ログ、milestone 履歴、PR 単位の一時 issue、agent prompt は docs/ の文書に置かない。

### 意図的に許容する重複

次の重複だけを意図的に許容します。「正」を更新したら、同じ変更で複製先も更新してください。この一覧にない重複は執筆時に解消します。

1. install.sh ワンライナー: 正 = usage.md、複製 = README.md。リリース版番号を含むため、release PR で両方を更新する(手順は [release.md](release.md))。
2. 最小クイックスタート(image-based の例): 正 = usage.md、複製 = README.md。README 側は最小形に切り詰めてよい。
3. セキュリティ警告の要約(任意コード実行・credential 到達性): 正 = usage.md、複製 = README.md。
4. ホスト要件の要約 bullet: 正 = specification.md、複製 = README.md / usage.md(要約形)。
5. 各文書冒頭の 1 行責務宣言と相互リンク: 責務定義の正 = この節。
