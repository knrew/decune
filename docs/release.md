# decune のリリース手順

この文書はメンテナー向けのリリース手順書です。利用者向けのインストール手順は [usage.md](usage.md)、開発・検証手順は [development.md](development.md) を参照してください。

## 方針

- 公式配布は GitHub Releases のビルド済みアーカイブです。`Cargo.toml` は `publish = false` のため、crates.io publish は行いません。配布物の契約(公式導線、アーカイブ内容、release asset、検証手段、`decune --version` の表示規則)は [specification.md 11 章](specification.md#11-配布の契約)を正とします。
- タグは `vMAJOR.MINOR.PATCH` 形式にします。pre-release は `v0.1.1-rc.1` のように SemVer の pre-release 表記を使います。
- リリースノートは GitHub Releases の generated release notes を使います。必要な見出しや除外条件が増えた場合は、GitHub の release notes 設定で調整します。
- 成果物は GitHub Actions 上で作り、ローカルで作ったバイナリは配布しません。
- 可能なら署名付きの注釈付きタグ (annotated tag) を使います。署名環境がない場合も軽量タグではなく注釈付きタグを使います。
- 通常開発中はリポジトリルートの `Cargo.toml` の `[workspace.package]` の `version` を直近リリース版のままにし、リリース PR でだけリリース予定版へ更新します。タグから作るリリース成果物の `decune --version` は、ビルドメタデータの付かない `decune MAJOR.MINOR.PATCH` として確認します。

## 通常フロー

1. リリース PR を作成します。
   - リポジトリルートの `Cargo.toml` の `[workspace.package]` の `version` をリリース予定版へ更新します。
   - ドキュメントのバージョン表記を同じ版へ揃えます。更新箇所は次の 2 か所です。更新箇所が増減した場合は、この一覧も更新します。
     - [README.md「インストール」](../README.md#インストール)の install.sh ワンライナー
     - [usage.md「インストール」](usage.md#インストール)の install.sh ワンライナーと手動アーカイブ手順の `version=`
   - 公開挙動、CLI オプション、設定キー、セキュリティ境界が変わる場合は `docs/specification.md` と関連する利用者向けドキュメントも更新します。

2. リリース PR で標準検証を通します。

3. リリース PR をマージします。

4. リリースコミットにタグを作成して push します。

   署名付きタグを使える場合:

   ```sh
   git fetch origin
   git checkout master
   git pull --ff-only origin master
   cargo run --locked -p xtask -- release-preflight --tag v0.1.0 --version 0.1.0
   git tag -s v0.1.0 -m "decune v0.1.0"
   git push origin v0.1.0
   ```

   署名環境がない場合:

   ```sh
   git fetch origin
   git checkout master
   git pull --ff-only origin master
   cargo run --locked -p xtask -- release-preflight --tag v0.1.0 --version 0.1.0
   git tag -a v0.1.0 -m "decune v0.1.0"
   git push origin v0.1.0
   ```

5. `Release` ワークフローを監視します。

   ```sh
   gh run list --workflow release.yaml --limit 5
   gh run watch <run-id>
   ```

   ワークフローはタグの事前検査、4 ターゲットのアーカイブのビルド、スモークテスト、`SHA256SUMS`、`release-manifest.json`、artifact attestation、GitHub の generated release notes 付きの GitHub Release 作成を行います。

   Release 公開後に generated release notes の本文を確認し、必要に応じて GitHub 上で利用者向けに整えます。

6. 公開後の確認を行います。

   ```sh
   gh release view v0.1.0
   gh release download v0.1.0 --pattern SHA256SUMS --pattern "decune-v0.1.0-*.tar.gz" --dir /tmp/decune-v0.1.0
   cd /tmp/decune-v0.1.0
   sha256sum -c SHA256SUMS
   ```

   GitHub CLI が使える環境では artifact attestation も確認します。

   ```sh
   gh attestation verify decune-v0.1.0-x86_64-unknown-linux-musl.tar.gz -R knrew/decune
   ```

7. インストーラーを実環境で確認します。

   ```sh
   tmpdir="$(mktemp -d)"
   curl -fsSL https://raw.githubusercontent.com/knrew/decune/v0.1.0/scripts/install.sh | sh -s -- --version 0.1.0 --dir "$tmpdir"
   "$tmpdir/decune" --version
   ```

## 失敗時の扱い

- タグの push 後にワークフローが失敗し、GitHub Release が未公開の場合は、原因を修正したコミットを作ってから新しいパッチバージョンまたは pre-release のタグを切ります。既に共有されたタグの移動は避けます。
- GitHub Release が公開済みで成果物に問題がある場合は、該当リリースを非公開化または説明を追記し、修正版を新しいバージョンで出します。
- crates.io に publish していないため、`cargo yank` は通常フローには含めません。

## crates.io publish を導入する場合

将来 crates.io を公式配布に含める場合は、`publish = false` を外すだけでは不十分です。Cargo の publish 要件に合わせてメタデータ、公開対象ファイル、パッケージサイズを再確認し、`cargo publish --dry-run` または `cargo package` と `cargo package --list` で公開内容を確認してから publish します。
