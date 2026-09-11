# Ready-to-post drafts（发布稿：复制粘贴即可，发布前把占位替换为真实数据）

> 红线提醒：这些是给账号持有人本人手动发布的；发布后以作者身份回复评论。
> 不要用小号互动，不要批量张贴。

---

## 1. r/LocalLLaMA 帖子

**标题：**
I built a free tool that finds duplicate GGUFs across Ollama, LM Studio and HuggingFace — and collapses them with NTFS hardlinks (nothing breaks)

**正文：**

Like a lot of you I have models duplicated across tool-specific stores: Ollama keeps hash-named blobs, LM Studio keeps its own folder tree, and a file I downloaded manually lives somewhere else. Same 5 GB quant × 3 copies = 15 GB.

I looked for a fix and found either "locators" (list the files, do nothing) or manual symlink surgery. So I built **GGUF Janitor** (Windows, free, single 6.5 MB exe, no installer):

- Scans Ollama (reads manifests, so you see `llama3:8b` instead of `sha256-…`), LM Studio (both old and new layouts), HuggingFace cache, plus any folders you add
- Groups content-identical files: exact size → full-streamed XXH3 hash → head/tail 4 KiB byte comparison. No hash-collision gambling.
- Default action: replace duplicates with an **NTFS hardlink** — every tool keeps seeing its own file, the disk stores one physical copy
- Alternatives: Recycle Bin, or hash-verified archive move to another drive (with an undo manifest + `undo` command)
- Also parses GGUF headers and estimates whether each model fits your RAM/VRAM (GQA-aware KV-cache estimate)
- Dry-run by default, skips files that are currently in use

It's open source (Rust, MIT): <仓库链接>
Download: <Release 链接>

Honest limits: hardlinks can't cross drives (NTFS rule — use the archive move for that), and split GGUF shards are never "deduped" against each other. The free version dedupes files up to 1 GiB; a one-time license removes that if you find it useful. Everything else (scan, report, fit check) is free without limits.

Happy to answer questions / take bug reports here or on GitHub.

---

## 2. Show HN

**标题：** Show HN: GGUF Janitor – reclaim disk space from duplicate local LLM models

**正文：**

Local LLM tools each keep private copies of model files: Ollama's blob store, LM Studio's model tree, and loose GGUFs add up to 2–3× the real size of your collection.

GGUF Janitor (Windows, Rust, single exe) scans those stores, identifies content-identical files (size + full XXH3 + head/tail byte check), and replaces duplicates with NTFS hardlinks so every tool keeps working while the disk stores one copy. There's also a hash-verified "archive move" with an undo manifest, a GGUF header parser, and a RAM/VRAM fit estimator.

Safety was the design center: re-verify before every action, never permanent-delete (Recycle Bin only), skip locked files, dry-run first.

Stack: egui for the GUI, no runtime deps. MIT, free; a paid license just removes the 1 GiB/file dedupe cap.

Repo: <仓库链接> — feedback welcome, especially on the Windows models you use (Ollama / LM Studio / llama.cpp / HF cache layouts).

---

## 3. GitHub Issue 补充评论模板（Ollama #1450 / #8506 / #13760 — 一次性、信息性）

> FYI: since this keeps coming up — I built a third-party tool that does this without waiting on Ollama itself: it scans the blob store + LM Studio/HF caches, hash-verifies duplicates, and replaces them with NTFS hardlinks so both tools keep working. Windows, MIT, free: <仓库链接>. Not affiliated with Ollama; just sharing in case it helps anyone here.

---

## 4. 新需求证据（更新于 03:45）

- r/LocalLLM："Local model registry to solve duplicate GGUFs across apps?"（用户在手动 symlink）
- 与此前 r/LocalLLaMA 三帖 + Ollama #1450/#8506/#13760 + Jan #1185 共同构成长期、跨社区的真实需求。
