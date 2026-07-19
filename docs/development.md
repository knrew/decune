# decune の開発

この文書は、コントリビューター向けの環境構築、検証、リリース成果物の作成コマンドをまとめます。利用手順は [usage.md](usage.md)、公開挙動は [specification.md](specification.md)、maintainer 向けのリリース手順は [release.md](release.md) を参照してください。

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

コンテナ側ツールの bundle を埋め込まない build は公式インストール手順ではありません。軽いローカル確認だけならインストールせず、通常の Cargo コマンドを使ってください。

## 開発ビルドのバージョン表示

リリース直後の開発中は、次のリリース番号を先に固定しないため root `Cargo.toml` の `[workspace.package]` version は直近リリース版のままにします。`decune --version` は clean な release tag では `decune 0.1.0` のように表示し、tag 外や未コミット変更を含む build では `decune 0.1.0+g<commit>` または `decune 0.1.0+g<commit>.dirty` のように Git 由来の build metadata を付けます。

Git 情報を取得できない source build では `+source` suffix を付けます。この suffix は表示用であり、Docker label などの内部 metadata には Cargo が解決した package version を使います。

## 標準検証

通常の変更では formatting と lint を確認します。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets
NPM_CONFIG_CACHE=/tmp/decune-npm-cache npx -y markdownlint-cli2@0.22.1 --config .markdownlint.yaml README.md AGENTS.md docs/*.md
```

Shell script formatting は `.editorconfig` の shell 用設定を `shfmt` が読む形で管理します。

対象を絞った test は package/module/test filter を指定して実行します。

```sh
cargo test -p decune <module_or_test_name>
```

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

## ドキュメント管理

- [README.md](../README.md) は概要、インストール、クイックスタート、主要リンクに絞る。
- 操作手順を中心にした利用者向け説明は [usage.md](usage.md) に置く。
- [specification.md](specification.md) は公開挙動、CLI の契約、設定スキーマ、セキュリティ境界の正本として保つ。
- コントリビューター向けの環境構築、検証、リリース成果物の作成コマンドはこの文書に置く。
- maintainer 向けのリリース runbook は [release.md](release.md) に置く。
- プロジェクト用語は [glossary.md](glossary.md) に揃える。

公開挙動、CLI option、設定 key、セキュリティ境界を変更した場合は、同じ変更で関連する利用者向けドキュメントも更新してください。
