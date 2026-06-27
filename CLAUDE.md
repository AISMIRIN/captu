# captu 開発ガイド

地デジ録画TSファイルから字幕テキストを抽出・索引化し、文言検索 → コンタクトシートでフレーム選定 → JPEG共有/コピーを行うWebアプリ。
設計の詳細は `docs/spec.md` を参照。

---

## モジュール構成

```
src/
├── main.rs          # axumサーバ起動、起動時スキャン (routes::build_router() を呼ぶ)
├── lib.rs           # クレートルート (モジュール宣言)
├── config.rs        # config.toml 読み込み
├── db.rs            # SQLiteスキーマ・接続プール
├── ingest.rs        # TSスキャン・取り込みオーケストレーション
├── scheduler.rs     # 定期スキャン (tokio-cron-scheduler, IngestGuard)
├── ts/
│   ├── mod.rs
│   ├── b24.rs       # ARIB STD-B24テキストコーデック (decode_arib_b24) — EPG専用pure-Rust
│   ├── epg.rs       # EIT/EPGパーサ → EpgInfo
│   ├── pes.rs       # ARIB字幕PESデマクサ (find_caption_pid, demux_caption_pes)
│   └── subtitle.rs  # aribcaption FFI字幕抽出・on-demand PNG描画
│                    #   Caption { pts_start_ms, pts_end_ms, text }
├── media/
│   ├── mod.rs
│   └── capture.rs   # ffmpeg 単一パスサムネ生成 (stock ffmpeg)
│                    #   scale → select → 字幕PNGオーバーレイ → JPEG (1コマンド)
├── routes/
│   ├── mod.rs       # AppState, build_router(), display_title(), fmt_ms(), like_escape()
│   ├── search.rs    # GET /, GET /search
│   ├── contact.rs   # GET /contact/{id} (コンタクトシート)
│   ├── capture.rs   # GET /thumb/{id}/{n}, GET /full/{id}/{n}, POST /select/{id}/{n}, POST /recapture/{id}
│   ├── episodes.rs  # GET /api/episodes
│   ├── tags.rs      # POST /caption/{id}/tags, POST /caption/{id}/tags/delete, GET /api/tags
│   └── ingest.rs    # GET /ingest/status, GET /ingest/files, GET /ingest/file/{id}
│                    #   POST /ingest/clear/{id}, POST /reingest/{id}
└── bin/
    ├── extract.rs    # 診断CLI: TSから字幕/EPGをダンプ
    └── ingest_cli.rs # 本番CLI: スキャン・再取り込み

crates/
├── aribcaption-sys/ # libaribcaption raw FFI bindings (bindgen + vendor submodule)
└── aribcaption/     # safe wrappers: Context / Decoder / Renderer / RenderedImage
                     # Decoder::set_replace_msz_fullwidth_japanese でcaptu固有設定を制御

ui/
├── templates/       # askamaテンプレート (askama.toml の dirs でテンプレートディレクトリ指定)
│   ├── layouts/     # base.html
│   ├── macros.html  # 共有マクロ
│   ├── pages/       # index.html / contact.html / ingest_status.html / ingest_files.html / ingest_file.html
│   └── fragments/   # episodes.html / search_results.html / tag_options.html / tags.html
└── static/
    ├── app.js       # フレーム選択・JPEG共有/コピー/ダウンロード (contact系)
    └── search.js    # 検索フィルタ・タグチップ・セッション復元 (index系)
```

## キャッシュ構成

```
cache/{ts_stem}/
  captions.pes           # ARIB字幕PESブロブ (取り込み時に保存)
  sub/{caption_id}.png   # 字幕PNG (on-demand描画、初回アクセス時に生成)
  preview/{caption_id}.jpg  # 字幕なし単フレームJPEG (検索結果プレビュー用、初回アクセス時に生成)
  thumbs/
    {caption_id}_{n:02}.jpg  # コンタクトシートJPEG (縮小表示用、初回アクセス時に生成)
  full/
    {caption_id}_{n:02}.jpg  # フル解像度JPEG (DL/共有用、初回アクセス時に生成)
```

## 技術規約

- ffmpegのシークは必ず `-ss` を `-i` の前に置く (NAS越しのため)
- sqlxは `query!` マクロを使う (コンパイル時クエリ検証)。`.sqlx/` をgit管理に含める
- テンプレートはaskama (コンパイル時検証)
- TSの取り込みは `tokio::spawn` でバックグラウンド実行しAPIをブロックしない
- PESブロブ・字幕PNG・JPEGはキャッシュ済みなら再生成しない
- **ビルド・テスト・抽出は `scripts/dev.sh` 経由** (root所有ファイル回避)

## 進め方の原則

- 実装したら必ず検証コマンドを実行し、結果を報告してから次へ進む
- 検証が失敗したら原因を報告し修正案を提示する。勝手に大きく設計変更しない
- 個人的・環境固有の作業メモは git 管理外の `CLAUDE.local.md` に置く (自動ロードされる)
- **改修は feature ブランチで行い、PR → CIグリーン → main マージ** (main 直 push は緊急時・リリースのみ)
- PR作成前に `/sync-docs` を実行してドキュメントずれを解消する

## 検証チェックリスト

実装完了の条件は以下すべてがCIと同等に通ること（`scripts/dev.sh` 経由で実行）：

```bash
# フォーマット確認
scripts/dev.sh fmt --all --check

# Clippyワーニングなし (CIと同じフラグ)
scripts/dev.sh clippy --workspace --all-targets -- -D warnings

# テスト全通過
scripts/dev.sh test
```

いずれかが失敗した場合は、pushやPR作成の前に必ず修正すること。

## テストカバレッジ運用 (二層モデル)

**強制集合**（テスト可能なコード）→ `scripts/cov.sh fail` でしきい値ゲート。  
**免除集合**（ffmpeg / FFI / サーバ起動など）→ `#[cfg_attr(coverage_nightly, coverage(off))]` を付与し計測から除外。別途 integration / 手動確認。  
属性の**直上**に除外理由コメントを**必ず**付ける（下記フォーマット参照）。

```bash
# HTMLレポートで未カバー行を赤表示
scripts/cov.sh

# テキストサマリ (行カバレッジ % のみ)
scripts/cov.sh summary

# CI相当のしきい値チェック (下回ると exit 1)
scripts/cov.sh fail
```

### 新機能を追加したとき
- **テスト可能なコード** → ユニットテストを追加する。`scripts/cov.sh fail` でしきい値割れがないか確認。
- **テスト困難なコード**（外部プロセス・FFI・ライブDB依存など）→ 属性の直上に除外理由コメントを付けてから `#[cfg_attr(coverage_nightly, coverage(off))]` を付ける。  
  コメントは英語 1〜2行で「テスト困難な理由」と「別途どう確認するか」を書く。  
  この diff がレビュー上で「ここは別途確認」の合意フックになる。

  ```rust
  // <reason why unit testing is impractical, e.g. spawns ffmpeg / FFI / live DB / server boot>.
  // Confirmed separately (integration / manual). Not included in the coverage gate.
  #[cfg_attr(coverage_nightly, coverage(off))]
  pub async fn my_handler(...) {
  ```

**しきい値の調整**: `scripts/cov.sh summary` で実測後、数ポイント下げた値を  
`scripts/cov.sh`（`THRESHOLD` 変数）と `.github/workflows/ci.yml`（`--fail-under-lines`）の  
両方に反映する。テストが増えたら引き上げる方針。

## ブランチ戦略

### 作業開始手順（必須）

新しい作業を始める前に必ず **`/prep-branch`** を実行する（main 最新化 → 新規 feature 派生 or 既存ブランチに main 取り込み）。  
引数: なし=新規ブランチ（作業内容から命名）／`current`=現在のブランチ継続／`feature/xxx`=指定ブランチ。いずれも main は対象にしない。詳細手順はスキル側に定義。

### PR フロー

```
git switch -c feature/xxx   # main から派生して作業
# 実装 → /sync-docs → 検証 (dev.sh fmt/clippy/test)
git push -u origin feature/xxx
# GitHub で PR 作成 → CI グリーン → squash merge → main
```

main ブランチ保護: PR 必須・CIグリーン必須 (admin は bypass 可)。
Dependabot PR は CI 通過後に squash 自動マージ。

## リリース手順

`scripts/release.sh` が cargo-release をコンテナ内で実行し、バージョンbump・commit・タグ・push を一括処理する。
タグ push 後は CI が自動で GitHub Release を生成する。

```bash
# main 上で実行。必ずクリーンな状態で。
scripts/release.sh patch   # 0.1.0 → 0.1.1
scripts/release.sh minor   # 0.1.0 → 0.2.0
scripts/release.sh major   # 0.1.0 → 1.0.0
scripts/release.sh 1.2.3   # 明示指定
```

## エディタ (rust-analyzer) の既知問題

askama 0.16 がループ変数 ident に subspan を付与するようになった影響で、
rust-analyzer の proc-macro サーバが `#[derive(Template)]` を展開する際に
`proc-macro panicked: "..." is not a valid identifier` を出す。

- **rustc / CI は無影響**（`scripts/dev.sh` はすべてグリーン）。
- `.zed/settings.json` の `procMacro.ignored` で askama の Template 展開をスキップして回避済み。
- askama または rust-analyzer の修正で解消したら設定を撤去してよい。

## コミット規約

形式: `type: 説明` (一行目) + 任意の本文 + `Co-Authored-By` トレーラー

```
type: short description

- 変更点の箇条書き (大きい変更のみ)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
```

**type 一覧**

| type | 用途 |
|---|---|
| `feat` | 新機能 |
| `fix` | バグ修正 |
| `refactor` | 動作変更を伴わないコード整理 |
| `test` | テスト追加・修正 |
| `docs` | ドキュメント・コメントのみ |
| `ci` | CI/CD・Dockerfile・スクリプト |
| `perf` | パフォーマンス改善 |
| `update` | 依存クレート更新・設定更新など |

**ルール**
- 説明は小文字始まり、命令形
- 本文は大きな変更のときのみ追加（小さい fix は一行で十分）
- Claudeが作成したコミットには必ず `Co-Authored-By` を付ける
