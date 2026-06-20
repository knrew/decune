# decune のリリース手順

この文書は maintainer 向けのリリース runbook です。利用者向けのインストール手順は [usage.md](usage.md)、開発・検証手順は [development.md](development.md) を参照してください。

## 方針

- 公式配布は GitHub Releases のビルド済みアーカイブです。`Cargo.toml` は `publish = false` のため、v0.1 では crates.io publish は行いません。
- tag は `vMAJOR.MINOR.PATCH` 形式にします。pre-release は `v0.1.1-rc.1` のように SemVer の pre-release suffix を使います。
- release notes は GitHub Releases の generated release notes を使います。必要な見出しや除外条件が増えた場合は、GitHub の release notes 設定で調整します。
- 成果物は GitHub Actions 上で作り、ローカルで作ったバイナリは配布しません。
- 可能なら署名付き annotated tag を使います。署名環境がない場合も lightweight tag ではなく annotated tag を使います。
- 通常開発中は `Cargo.toml` の version を直近リリース版のままにし、release PR でだけリリース予定版へ更新します。tag から作る release artifact の `decune --version` は build metadata suffix なしの `decune MAJOR.MINOR.PATCH` として確認します。

## 通常フロー

1. release PR を作成します。
   - `Cargo.toml`、workspace member の `Cargo.toml`、README、usage のバージョン表記をリリース予定版へ揃えます。
   - 公開挙動、CLI option、設定 key、security boundary が変わる場合は `docs/specification.md` と関連する利用者向けドキュメントも更新します。

2. release PR で標準検証を通します。

   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-features --all-targets -- -D warnings
   cargo run --locked -p xtask -- workspace-test
   cargo run --locked -p xtask -- compose-integration
   cargo run --locked -p xtask -- release-preflight --tag v0.1.0 --version 0.1.0
   ```

   Docker / Compose integration test を実行できない環境では、未実行範囲を PR と最終報告に明記します。

3. release PR を merge し、default branch の CI が成功していることを確認します。

4. release commit に tag を作成して push します。

   署名付き tag を使える場合:

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

5. `Release` workflow を監視します。

   ```sh
   gh run list --workflow release.yaml --limit 5
   gh run watch <run-id>
   ```

   workflow は tag の preflight、4 target の archive build、smoke test、`SHA256SUMS`、`release-manifest.json`、artifact attestation、GitHub generated release notes 付きの GitHub Release 作成を行います。

   Release 公開後に GitHub generated release notes の本文を確認し、必要に応じて GitHub 上で利用者向けに整えます。

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

- tag push 後に workflow が失敗し、GitHub Release が未公開の場合は、原因を修正した commit を作ってから新しい patch version または pre-release tag を切ります。既に共有された tag の移動は避けます。
- GitHub Release が公開済みで成果物に問題がある場合は、該当 release を非公開化または説明を追記し、修正版を新しい version で出します。
- crates.io に publish していないため、`cargo yank` は v0.1 の通常フローには含めません。

## crates.io publish を導入する場合

将来 crates.io を公式配布に含める場合は、`publish = false` を外すだけでは不十分です。Cargo の publish 要件に合わせて `description`、`license`、`repository`、`readme` などの metadata を整備し、`cargo publish --dry-run` または `cargo package` と `cargo package --list` で公開内容を確認してから publish します。
