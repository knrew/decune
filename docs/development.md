# decune の開発

この文書は、コントリビューター向けの環境構築、検証、リリース成果物の作成コマンド、ドキュメント執筆規約をまとめます。利用手順は [usage.md](usage.md)、公開挙動は [specification.md](specification.md)、内部実装の説明は [internals.md](internals.md)、メンテナー向けのリリース手順は [release.md](release.md) を参照してください。

## ソースからのローカルインストール

ソースコードからのインストールは、公式のローカルインストール手順です。Git credential forwarding と port forwarding に必要な container-side tools をビルドし、ホスト側バイナリに埋め込んでから `decune` をインストールします。

Rust のツールチェーンは `Cargo.toml` の `rust-version` 以上の stable を使います。decune の MSRV は現行 stable への追従方針で、古い Rust のマイナーバージョンの長期サポートは対象外です。

Linux 用の container-side tools をビルドするため、以下の Rust のターゲットが必要です。

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
```

xtask のインストールコマンドを実行します。

```sh
cargo run --locked -p xtask -- install --locked
```

このコマンドは `target/decune-xtask/container-tools-bundle` に `git-credential-decune`、`decune-forward-agent`、コンテナ側 `decune` の 3 ツールを 2 プラットフォーム向けにビルド / 検証し、6 artifact の bundle を埋め込んだホスト側 `decune` を `cargo install` します。コンテナ側 `decune` の Cargo の binary target は `decune-container-cli`、bundle 内の artifact 名は `decune` です。bundle のビルド / 検証だけを個別に確認する場合は、以下を実行します。

```sh
cargo run --locked -p xtask -- build-container-tools --locked
cargo run --locked -p xtask -- check-container-tools
```

container-side tools bundle を埋め込まないビルドは公式インストール手順ではありません。軽いローカル確認だけならインストールせず、通常の Cargo コマンドを使ってください。bundle の埋め込みの仕組み、ビルド時の内部環境変数、開発用の上書き `DECUNE_CONTAINER_TOOLS_DIR` の説明は [internals.md](internals.md#6-container-side-tools-bundle-と実行時の配置) にあります。

## 開発ビルドのバージョン表示

リリース直後の開発中は、次のリリース番号を先に固定しないためリポジトリルートの `Cargo.toml` の `[workspace.package]` の `version` は直近リリース版のままにします。`decune --version` はクリーンなリリースタグでは `decune 0.1.0` のように表示し、タグ外や未コミット変更を含むビルドでは `decune 0.1.0+g<commit>` または `decune 0.1.0+g<commit>.dirty` のように Git 由来のビルドメタデータを付けます。

Git 情報を取得できないソースビルドでは `+source` の接尾辞を付けます。この接尾辞は表示用であり、Docker のラベルなどの内部メタデータには Cargo が解決したパッケージバージョンを使います。公式配布成果物のバージョン表示規則の正は [specification.md 11 章](specification.md#11-配布の契約)です。

## 標準検証

通常の変更では整形と lint を確認します。lint には `markdownlint-cli2`、`shellcheck`、`shfmt`、`yamllint` を使います。バージョンは CI(`.github/workflows/ci.yaml`)に合わせます。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets
markdownlint-cli2 --config .markdownlint.yaml README.md docs/*.md
bash .github/scripts/lint-sh-bash.sh # shellcheck + shfmt
yamllint .
```

対象を絞ったテストはパッケージ / モジュール / テストのフィルタを指定して実行します。

```sh
cargo test -p decune <module_or_test_name>
```

## テストの検証範囲

decune のテストの網羅範囲は、少なくとも以下の挙動グループを含めます。公開挙動を変更する場合は、該当するグループのテストも同じ変更で更新してください。

- image-based / Dockerfile-based / Docker Compose-based の `up` / `rebuild` / `down` / `remove`。
- Dockerfile のビルドの入力、`.dockerignore` の扱い、`--no-cache`、`--pull`、未対応の Dockerfile / コンテキストの組み合わせ。
- Compose の `dockerComposeFile`、`service`、`runServices`、profiles、複数ファイルのマージ、decune-generated Compose override の挙動、プロジェクト削除の安全性。
- Feature の解決、lock の扱い、メタデータのマージ、オプションの環境変数 / 既定値の扱い、local Feature の制約、UID/GID 同期、entrypoint shim の挙動。
- dotfiles、マウント、lifecycle command、decune hook、シェル接続、lifecycle の二重実行防止。
- manual/automatic port forwarding、published port の警告 / エラー、sidecar の明示的な転送、TCP のみ対応の挙動。
- credential forwarding、トークンの redaction、状態の修復、リソース名のサニタイズ、秘密情報の漏えいの回帰テスト。
- decune container CLI の image/Dockerfile/Compose の primary、コマンド / stdio / exit の組み合わせ、UID / sidecar の構成、attached/detached、有効時の lifecycle、symlink のフォールバック、forwarding の集約・daemon handoff、live なワークスペース / ホスト側パスの非参照。

## テスト fixture 管理

テストで使う長いシェルスクリプト、TOML、JSON、YAML、Dockerfile は、原則として `tests/fixtures` 配下に置き、テスト実行時に読み込みます。新規のテスト fixture に `include_str!` / `include_bytes!` は使いません。

配置は用途別に揃えます。

- `tests/fixtures` 配下のシェル fixture のディレクトリ / ファイル名は `kebab-case` に揃える。
- Docker Compose の統合テスト用のワークスペース fixture は `tests/fixtures/compose/` に置く。
- CLI harness の共通 fixture は `tests/fixtures/cli/harness/` に置く。
- 複数のテストで使う fake の `docker` / `gh` / `git` / `curl` などは `tests/fixtures/cli/fake-bin/` に置く。
- モジュール固有の長い fake コマンドは `tests/fixtures/cli/<module>/<case>.sh` に置く。
- 複数ファイルで構成される CLI のワークスペース fixture は `tests/fixtures/cli/workspaces/<module>/<case>/` に置く。

テストから fixture を読む場合は `tests/support/support.rs` のヘルパーを使います。fixture のパスと一時ワークスペースのパスは絶対パスと `..` を拒否します。一時ワークスペースへ fixture を置く場合は `TempWorkspace::copy_fixture_dir`、`TempWorkspace::write_fixture_file`、`TempWorkspace::write_fixture_template`、`TempWorkspace::write_executable_fixture` を使ってください。

テストがホスト側に作る一時ファイル / ディレクトリは、原則として `tempfile::TempDir`、`tempfile::NamedTempFile`、またはそれらを所有する共通ヘルパーで管理します。`std::env::temp_dir()` から直接パスを組み立て、テストの先頭や末尾だけで手動削除する構成にはしません。一時パスをコマンド、ワークスペース、非同期処理へ渡す場合は、その利用が終わるまで所有者を保持してください。通常は所有者の `Drop` に後始末を任せ、後始末の失敗自体をテストの失敗として確認する必要がある場合だけ `TempDir::close()` などを使います。

複数のプロセスが同じ inode をロックするためのファイルなど、プロセス間で安定したパスが必要な場合は例外です。この場合は一時領域をテストごとに分離せず、直接パスが必要な理由と後始末の方針をコードに明記します。

CLI のテストで fake のホストコマンドを `PATH` に置く場合は、モジュール内限定のヘルパーを増やさず `tests/cli/harness.rs` の `fake_command_path`、`fake_docker_path`、`fake_path_with_commands` などを使います。

実行時の値が必要な fixture では `__HOST_PORT__` のようなプレースホルダーを置き、`write_fixture_template` で展開します。プレースホルダーを指定したのに fixture 内に存在しない場合はテストのヘルパーが失敗します。数行だけの異常系入力、assert の近くにある方が意図を読みやすいマーカー / 設定はインラインのままでも構いません。

動的に `source` する必要がある箇所だけ、該当行の直前に `# shellcheck disable=SC1090` を付けます。Git 上のシェル fixture は通常 `100644` のままにし、実行が必要なテストでは `TempWorkspace::write_executable_fixture` で一時ワークスペース側に `0755` として書き出します。

## Docker を使う検証

Docker を使うテストを含む全体検証には、Docker デーモンと Docker Compose v2 プラグインへ接続できる環境が必要です。

```sh
cargo run --locked -p xtask -- workspace-test
cargo run --locked -p xtask -- compose-integration
```

これらのコマンドは container-side tools のビルドと `cargo test --verbose` の進捗を逐次出力します。テスト本体の標準出力は Rust のテストハーネスの既定どおりキャプチャされ、失敗時に表示されます。

Compose の統合テストだけを実行する場合:

```sh
docker version
docker compose version
cargo run --locked -p xtask -- compose-integration
```

`compose_integration` の Docker を使うテストは `#[ignore]` として定義します。通常の単体テストでは実行されず、`compose-integration` が Docker/Compose の利用可否を確認したうえで、`#[ignore]` の統合テストを単一のテストスレッドで実行します。

この経路は通常の Compose のシナリオに加え、実際の container-side tools bundle の 3 ツール × 2 プラットフォームをビルド / 検証し、decune container CLI の image-based / Dockerfile-based / Docker Compose-based の E2E を実行します。decune container CLI の E2E は attached な `up` のプロセスと別の `docker exec` のプロセスを使い、クエリ、UID / sidecar の構成、lifecycle、forwarding の集約と daemon handoff、サニタイズ済みの開示境界を実際の Docker リソースで確認します。

## リリース成果物

配布アーカイブを手元で作成します。正式リリースでは、タグの push 後に GitHub Actions の `Release` ワークフローが同じ `xtask` を使って成果物を作成します。タグ作成から公開後確認までの手順は [release.md](release.md) を参照してください。

```sh
cargo run --locked -p xtask -- dist \
  --target x86_64-unknown-linux-musl \
  --version 0.1.0 \
  --locked
```

チェックサムとリリースマニフェストを生成します。

```sh
cargo run --locked -p xtask -- checksum --dist-dir target/dist --version 0.1.0
cargo run --locked -p xtask -- release-manifest --dist-dir target/dist --version 0.1.0
```

生成済みのバイナリ成果物は Git リポジトリにコミットしません。

## ドキュメント構成と執筆規約

この節は、decune のドキュメント構成と責務分担の唯一の定義です。各文書の冒頭にある 1 行の責務宣言と相互リンクは、この節の要約です。

### 文書の責務

| 文書 | 責務 |
| --- | --- |
| [README.md](../README.md) | 概要と最短導線(初見の利用者向け) |
| [usage.md](usage.md) | 利用者向けのインストール、クイックスタート、日常操作の基本ガイド |
| [configuration.md](configuration.md) | decune config の使い方と挙動説明のガイド |
| [ports.md](ports.md) | port forwarding と published port の利用ガイド |
| [clone-isolation.md](clone-isolation.md) | 複数クローン同時利用(Compose clone isolation)のガイド |
| [specification.md](specification.md) | 公開挙動、CLI 契約、設定スキーマ、セキュリティ境界、diagnostic code 定義の正本(リファレンス兼務) |
| [internals.md](internals.md) | 内部実装の説明(非規範の内部設計ノート) |
| development.md(この文書) | コントリビューター向けの環境構築、検証、ドキュメント執筆規約 |
| [release.md](release.md) | メンテナー向けのリリース手順書 |
| [glossary.md](glossary.md) | 用語の定義 |

### 情報の置き場所

同じ情報は 1 つの文書だけを正とし、他の文書は要約とリンクに徹します。正と矛盾する記述を他の文書に持ち込まないでください。

- 公開挙動、CLI の契約、設定スキーマと既定値、セキュリティ境界、diagnostic code の発生条件の正は specification.md。
- インストールと日常操作の手順、信頼していないリポジトリでの推奨設定の正は usage.md。
- 設定(features / dotfiles / mounts / credentials / hooks / container.cli)の使い方・挙動説明の正は configuration.md。
- forwarding と published port の概念説明・使い分け、relocation / mapping の確認方法の正は ports.md。clone isolation の使い方の正は clone-isolation.md。
- diagnostic code への対処の正は ports.md と clone-isolation.md のトラブルシューティング。specification.md には対処を書かない。
- ソースからのインストール、検証コマンド、テストの網羅範囲の要求、ドキュメント執筆規約の正はこの文書。
- 実装定数、内部型名、内部環境変数、ランタイムファイルのレイアウトなど内部実装の説明は internals.md。非規範であることを明記し、挙動の約束は書かない。
- 配布物の契約(asset、検証手段、バージョン表示規則)の正は specification.md の配布章。リリース手順の正は release.md、開発ビルドのバージョン運用はこの文書。
- 用語の正は glossary.md。本文で用語を追加・変更した場合は glossary.md も更新する。

### 執筆規約

- 公開挙動、CLI オプション、設定キー、セキュリティ境界を変更した場合は、同じ変更で specification.md と関連する利用者向けドキュメントも更新する。
- 仕様の規範記述は specification.md だけに書く。ガイドに書いてよい仕様記述は、その場面の理解に必要な要約 1〜2 文と specification.md へのリンクまでとする。
- README と usage は仕様を要約できるが、仕様と矛盾する内容を持たない。仕様、README、usage、実装、テストが矛盾する場合は、暗黙に実装を正とせず、差分の意図を確認してから揃える。
- 実装作業ログ、マイルストーンの履歴、PR 単位の一時的な課題、エージェント用プロンプトは docs/ の文書に置かない。

### 意図的に許容する重複

次の重複だけを意図的に許容します。「正」を更新したら、同じ変更で複製先も更新してください。この一覧にない重複は執筆時に解消します。

1. install.sh ワンライナー: 正 = usage.md、複製 = README.md。リリース版番号を含むため、リリース PR で両方を更新する(手順は [release.md](release.md))。
2. 最小クイックスタート(image-based の例): 正 = usage.md、複製 = README.md。README 側は最小形に切り詰めてよい。
3. セキュリティ警告の要約(任意コード実行・credential 到達性): 正 = usage.md、複製 = README.md。
4. ホスト要件の要約の箇条書き: 正 = specification.md、複製 = README.md / usage.md(要約形)。
5. 各文書冒頭の 1 行責務宣言と相互リンク: 責務定義の正 = この節。
