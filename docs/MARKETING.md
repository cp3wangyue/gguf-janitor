# GGUF Janitor — Marketing Pack

> 内部文档。所有发布动作由账号持有人手动执行；本文件只准备素材。
> 发布红线：不群发广告、不刷评论、不冒充普通用户。

---

## 1. 定位

| 项 | 内容 |
|---|---|
| 产品名 | GGUF Janitor |
| 一句话卖点（EN） | Stop wasting disk on duplicate local LLM models. |
| 一句话卖点（中文） | 本地大模型重复文件清理器：找回被 Ollama / LM Studio 偷走的磁盘空间。 |
| 目标用户 | 在 Windows 上运行本地 LLM（Ollama / LM Studio / llama.cpp）的爱好者与开发者；有多块大硬盘、模型收藏膨胀的人 |
| 解决的痛点 | 每个工具各存一份模型副本（8GB 模型 × 3 工具 = 24GB 浪费）；没有跨工具的安全去重方案；不知道哪个模型能塞进自己的显存 |
| 关键差异化 | 硬链接去重（工具全部继续可用）+ 双重校验安全模型 + 显存适配估算 + 单文件便携 exe |

## 2. 英文介绍（README / Release / 商品页通用）

**Short (Gumroad subtitle):**
Find and safely reclaim the disk space your local AI tools are wasting. Hardlink-deduplicate GGUF models across Ollama, LM Studio and HuggingFace — without breaking anything.

**Long (Gumroad body / GitHub Release):**

Every local-LLM tool keeps its own private copy of every model. LM Studio downloads it, Ollama copies it into its blob store, llama.cpp needs it somewhere else — your 8 GB model quietly becomes 24 GB.

GGUF Janitor fixes this in one click:

- Scans Ollama, LM Studio, HuggingFace caches and any folders you add
- Finds **content-identical** duplicates (size + full XXH3 hash + head/tail byte check — no guesswork)
- Replaces duplicates with **NTFS hardlinks**: every tool keeps seeing its own file, the disk stores one physical copy
- Also supports Recycle Bin deletion and hash-verified archive moves (with undo manifest)
- Estimates whether each model fits your RAM / VRAM (GQA-aware KV-cache math)
- Single 6.5 MB portable exe. No installer, no admin, no telemetry.

Free tier dedupes files up to 1 GiB. Pro (€14.99, one-time, offline license key) removes the limit — 70B+ quants and FP16 shards included.

**FAQ (English):**
- *Is it safe?* Duplicates are re-verified byte-for-byte before any action; hardlinks keep every copy visible to every tool; deletes go to the Recycle Bin; locked files are skipped.
- *Does Ollama/LM Studio break after dedupe?* No. A hardlink is just a second name for the same file. Both tools keep reading "their" copy as usual.
- *Cross-drive?* NTFS hardlinks cannot span volumes — the tool tells you and offers a verified archive move instead.
- *Do you phone home?* No network access at all. License keys validate offline.

## 3. 中文介绍（国内渠道用）

**短版：**
本地跑大模型的都懂：LM Studio 下一份、Ollama 存一份、llama.cpp 又一份——8GB 的模型实际占 24GB。GGUF Janitor 全盘扫描这些"影子副本"，用 NTFS 硬链接把重复文件合并成一份物理存储，所有工具照常使用，磁盘立刻省一半。双重校验（全文件 XXH3 + 首尾字节比对），锁定的文件自动跳过，删除只进回收站。单文件 6.5MB，免安装。

**长版：** 同英文版结构，强调：安全模型（先校验后动作、回收站、undo manifest）、显存适配估算（帮你决定留哪个量化版本）、免费版可清理 ≤1GB 文件，Pro 一次性 ¥108 解锁不限大小。

**FAQ（中文）**：同英文 FAQ 对译，另加：
- *是删我的模型吗？* 不是。默认方案是硬链接合并，两个路径都在，只是磁盘只存一份数据。
- *支持跨盘去重吗？* 硬链接技术限制不能跨盘；跨盘的重复文件用"校验后归档移动"处理，移动前算完整哈希，移动后留 undo 清单。

## 4. 定价方案

- Free：扫描/报告/适配估算无限；去重仅限 ≤1GB 文件
- **Pro €14.99（¥108）一次性**：不限大小，0.x 全部更新
- 备选：Gumroad "pay what you want" ≥$9.99 测试转化；前 50 名早鸟 $9.99
- 源码开放（MIT）但 exe 的 Pro 门控随二进制分发——用户可自编译，付费买的是"省事 + 更新"

## 5. SEO 关键词

EN: gguf duplicate finder, ollama disk space, lm studio models folder huge, free disk space llm, gguf manager windows, dedupe ollama models, huggingface cache cleanup, local llm storage
中文: ollama 磁盘空间、模型 重复 文件、LM Studio 模型 清理、gguf 管理、本地大模型 硬盘、ollama 清理工具

## 6. 发布渠道（手动执行，逐个人工发布，禁止批量）

1. **GitHub**：仓库 + Release（今天完成）— 主分发点
2. **r/LocalLLaMA**：等有 10+ 下载验证后发 "I built a free tool that hardlink-dedupes GGUFs across Ollama/LM Studio" — 诚实标注免费版限制；回复评论为主
3. **r/ollama、r/LLM_Devs、r/StableDiffusion（safetensors 版上线后）**：在相关求助帖回答时提及（先解决对方问题，再附工具）
4. **GitHub Issues 精准触达**：Ollama #1450 / #8506 / #13760、Jan #1185 等已有求助线程——在该 issue 下补一条"工具方案已存在"的信息性评论（一次、中性语气、注明免费），这不算广告spam，是相关技术信息
5. **V2EX / 吾爱破解（注意规则）/ 酷安**：中文渠道，按各社区规则发分享帖
6. **小众软件 / 异次元软件**：投稿式提交
7. **Hacker News (Show HN)**：标题 "Show HN: GGUF Janitor – reclaim disk space from duplicate local LLM models"
8. **Gumroad Discover + Product Hunt**：商品页就绪后

## 7. 素材清单（待做）

- [ ] 主截图：GUI 扫描完成、显示 3 组重复 + "Reclaim 42.3 GB" 按钮（需要造大点儿的演示数据）
- [ ] GIF：扫描 → 点击 Hardlink → 磁盘占用下降
- [ ] before/after 面积图（TreeSize 类工具佐证磁盘占用变化）
- [ ] 512px 图标（当前是程序化生成的占位图标）

## 8. 上线检查清单

- [x] exe 数字签名：暂无（EV 证书成本高；发布时注明 SHA256 校验）
- [ ] Release zip 内附 SHA256SUMS.txt
- [ ] Gumroad 商品页（文案见上）+ 收款账户（**需要用户本人操作**）
- [ ] GitHub 仓库 About/Topics：`windows`, `ollama`, `lm-studio`, `gguf`, `disk-cleanup`, `rust`, `egui`
