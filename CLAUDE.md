# captu 開発ガイド

地デジ録画TSファイルから字幕テキストを抽出・索引化し、文言検索 → コンタクトシートでフレーム選定 → JPEG共有/コピーを行うWebアプリ。
設計の詳細は `docs/spec.md` を参照。

---

## モジュール構成

```
src/
├── main.rs          # axumサーバ、起動時スキャン、ルータ組み立て
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
│   ├── mod.rs       # AppState, display_title(), fmt_ms(), like_escape()
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
- 残フェーズは `plans/` を参照 (phase5-scheduler / phase7-ai-search / phase8-multimodal)

## 検証チェックリスト

実装完了の条件は以下すべてがCIと同等に通ること（`scripts/dev.sh` 経由で実行）：

```bash
# フォーマット確認
scripts/dev.sh fmt --check

# Clippyワーニングなし (CIと同じフラグ)
scripts/dev.sh clippy -- -D warnings

# テスト全通過
scripts/dev.sh test
```

いずれかが失敗した場合は、pushやPR作成の前に必ず修正すること。

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
