# build と release の手順

nix の devshell での build、`script/bundle-mac` による .dmg の作成、rebrand、install、GitHub の release の手順。experience（`dotfiles-private/experience/zed/zed-metal-runtime-shaders.md`、2026-08-30 時点）から移した。nix の devshell で Metal Toolchain が見えない理由は、knowledge の `nix-devshell-path-shadows-xcrun-and-bash-for-native-builds.md` にある。

zed のビルドは runtime_shaders feature 必須（nix devshell＋macOS 26 では Metal Toolchain が見えない）。bundle-mac は cargo を3回呼ぶので全部に注入。版番号は base 固定で -latex.N だけ上げる

zed を nix devshellでビルドすると、`gpui_macos` の
ビルドスクリプトで Metal シェーダコンパイルが失敗する:
`cannot execute tool 'metal' due to missing Metal Toolchain`。

**根本原因（2つ重なる）**:
1. macOS 26 (Darwin 25.x / Tahoe) では `metal` コンパイラ本体が分離され、
   `xcodebuild -downloadComponent MetalToolchain` で別途取得が必要。
2. nix devshell が `DEVELOPER_DIR`/`SDKROOT` を **nix の apple-sdk-15.5** に差し替える。
   Metal Toolchain は本物の `/Applications/Xcode.app` にしか入らないため、
   devshell 内の `metal` 呼び出しは toolchain を見つけられない。
   - 確認: `env DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcrun metal --version` は成功するが、
     nix の DEVELOPER_DIR 経由だと失敗する。
   - 注意: `xcodebuild -downloadComponent` を zed dir（direnv 有効）で実行すると DEVELOPER_DIR が
     nix SDK を指すため、本物の Xcode にインストールされない。インストール時も
     `env DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcodebuild -downloadComponent MetalToolchain` と明示する。

**How to apply（正攻法）**:
- 公式 nix ビルド（nix/build.nix の cargoExtraArgs）と同じく
  **`--features gpui_platform/runtime_shaders`** を付けてビルドする。
  これでシェーダはビルド時でなく実行時コンパイルになり、ビルド時に `metal` が不要になる。
  例: `cargo build -p zed --features gpui_platform/runtime_shaders`
  `cargo run` で起動するときも同じ feature を付け続けること（付けないと gpui を再ビルドして再び metal で失敗）。
- runtime_shaders を使わず build-time シェーダにしたい場合は、本物の Xcode の Metal Toolchain を
  入れたうえで DEVELOPER_DIR を本物 Xcode に向ける必要があるが、nix SDK との整合が崩れるため非推奨。

**`.dmg` バンドル（`script/bundle-mac`）での落とし穴**:
- `bundle-mac` は cargo を **3回**呼ぶ: ①`-p zed -p cli`（90行）②`-p remote_server`（93行、feature 統合を避けるため別 invocation）③`cargo bundle`（103行）。
  **runtime_shaders は ①②③ 全部に注入が必要**。②を忘れると remote_server 側で gpui_macos が feature 無しで再コンパイルされ metal で失敗する（一度ハマった）。
- `bundle-mac` 87行の `rustup target add $target_triple` は nix devshell に rustup が無く `set -e` で即死する。ホスト=ターゲットなので `command -v rustup && ... || true` に書き換えてスキップ。
- **git 同梱ステップ（`download_git`）が失敗する**（2026-07 時点）: dugite-native の tar に `bin/git` が無く `tar: Not found in archive`。git 同梱をスキップする（システム git にフォールバック。リリースノートに「git は同梱していません」と書く）。
- **`npm install --global dmg-license minimist`（dmg ライセンス埋め込み）が nix devshell で EACCES**（nix store へ書けない）。`npx --yes --package dmg-license --package minimist dmg-license script/terms/terms.json "$dmg"` に置き換える。
- コンパイル成功後の再実行はキャッシュが効いて数分で dmg まで到達する（失敗ステップだけ直して素直に全体を再実行してよい）。

**版番号方針（2026-07 確定）**: version は `crates/zed/Cargo.toml` の `<upstream-base>-latex.<N>`。**base（`1.10.0` = upstream 分岐点）は固定**し、上げるのは `-latex.N` カウンタだけ（base を上げると upstream が動いたように見える）。過去に誤って `1.10.1-latex` と base を上げた版があるが `1.10.0-latex.3` で是正済み。Cargo.lock の `name="zed"` version も揃える。

**rebrand の推奨は「bundle-mac に一時注入」（2026-07 実績）**: `cp ... cli`（zed/cli コピー）の直後・`sign_app_binaries` の直前に、app を `Zed Dev RaTeX(unofficial).app` に `mv` → `app_path` 更新 → PlistBuddy で `CFBundleIdentifier=dev.ratex.zed-dev`（**旧 `dev.yosukeiida.zed-dev-ratex` は個人名回避のため廃止**）/ `CFBundleName`/`CFBundleDisplayName=Zed Dev RaTeX(unofficial)` を Set、を挿入する。直後の `sign_app_binaries` の ad-hoc codesign が変更後 plist を署名するので、dmg 内 app が最初から rebrand 済みになる（別途 ditto/再署名不要）。dmg は `Zed-aarch64.dmg` として出るので `Zed-Dev-RaTeX-unofficial-<version>-aarch64.dmg` に cp リネームして release アセットにする。
- **`zedd` は bundle id ではなくアプリ「パス」で起動**（~/.zshrc:141-144 が `"/Applications/Zed Dev RaTeX(unofficial).app/Contents/MacOS/cli" --user-data-dir ".../ZedDevRaTeX"` を直接実行）。よって **bundle id を変えても zedd は無関係**。アプリのパスと data dir（`~/Library/Application Support/ZedDevRaTeX`）を保てば zedd はそのまま動く。（旧メモの「zedd は id で起動」は誤り。）手動 install 時は `xattr -dr com.apple.quarantine` で Gatekeeper 回避。
- **install は `/Applications` への `rm -rf` が安全ガードで自動拒否される**ため Claude からは実行不可。ユーザーに `!` プレフィックスで `rm -rf "/Applications/Zed Dev RaTeX(unofficial).app" && ditto <built app> <dst> && lsregister -f <dst>` を実行してもらう。built app は `target/aarch64-apple-darwin/release/dmg/Zed Dev RaTeX(unofficial).app`（新 id・新版）。
- 署名は Apple 証明書が無ければ自動で ad-hoc（`-`）。dev チャンネル成果物は base が `Zed Dev.app`、公式 stable と別データディレクトリで共存可。
- release フルビルドは ①だけで ~55 分。②以降も長い。編集は tracked file なので `git checkout script/bundle-mac` で戻す前提の一時変更にする。
- **gh release 手順（2026-07 実績）**: commit（feature + version 別コミット）→ `git push origin math-preview` → `gh release create v<version> --repo YosukeIida/zed --target math-preview --title "Zed Dev RaTeX (unofficial) <version>" --notes-file ... <dmg>`。即公開なら `--draft` 無し。notes に「git 非同梱・ad-hoc 署名・id dev.ratex.zed-dev」を明記。
