# PDF裁剪工具（Tauri 版）

把 PDF 中所有「矩形注释」（Square Annotation）标注的区域裁剪出来，
重新组合成一个新的 PDF。**全程矢量操作**——文字仍是文字，不会栅格化成图片。

> 这是 Python/Gradio 版（`pdf_annot_crop.py`）的 Rust + Tauri 重写版：
> 安装包体积从 ~106MB 降到 **~5MB**，启动更快，无 Python 依赖。

## 功能

- **矩形注释裁剪**：每个 Square 注释区域裁剪为独立内容块
  - 普通模式：每个注释单独一个区域
  - 扩展模式：同页所有注释取并集（外接矩形）整块裁剪
- **页边距裁剪**：每页去掉四周留白（36pt）
- **两种排版**：
  - 拼版排版（推荐）：像 Word 插图一样把所有区域流式排列到 A4 页面，直接打印
  - 独立分页：每个区域单独一页，页面尺寸 = 区域尺寸
- **页码范围**：支持 `1-3,5` 写法，留空 = 全部
- 跨平台：Windows（msi/exe）、macOS（dmg）、Linux（deb/AppImage）

## 项目结构

```
pdf-crop-tauri/
├── core/                     # 核心 PDF 裁剪逻辑（纯 Rust + lopdf，独立可测试）
│   └── src/lib.rs
├── src-tauri/                # Tauri 2 桌面壳（GUI + 命令）
│   ├── src/lib.rs            # crop_pdf_cmd 命令
│   ├── tauri.conf.json
│   └── icons/
├── ui/                       # 静态前端（纯 HTML/JS，无 npm 构建）
├── .github/workflows/
│   ├── ci.yml                # push/PR 自动跑核心单元测试
│   └── release.yml           # 推 v* 标签自动发布三平台安装包
└── Cargo.toml                # workspace
```

## 使用方式（最终用户）

1. 到 [Releases](../../releases) 下载对应平台安装包：
   - Windows：`PDF裁剪工具_x.y.z_x64_zh-CN.msi`
   - macOS：`PDF裁剪工具_x.y.z_aarch64.dmg`（Apple Silicon）或 `_x64.dmg`
   - Linux：`.deb` 或 `.AppImage`
2. 打开软件 → 选择 PDF → 选择排版方式 → 提交

## 开发与发布（开发者）

本机无需安装任何工具链，一切交给 GitHub Actions：

```bash
# 日常提交（CI 会自动跑测试）
git push origin main

# 发布新版本：打标签即可，Actions 会构建三平台安装包并创建 Release
git tag v0.1.0
git push origin v0.1.0
```

CI 说明：
- `ci.yml`：`cargo test -p pdf-crop-core`（核心逻辑单元测试）+ `cargo check`（壳编译检查）
- `release.yml`：测试通过后用 `tauri-apps/tauri-action` 构建并发布

## 裁剪原理（技术说明）

与 PyMuPDF 的 `show_pdf_page(clip=...)` 相同的 PDF 标准技法：

1. 源页面内容流包装为 **Form XObject**，`BBox` = 裁剪区域（限定可见范围）；
2. `Matrix = [1 0 0 1 -x0 -y0]` 把区域原点平移到坐标原点；
3. 新页面上用 `q W w cm /Form Do Q` 完成缩放与摆放（拼版模式一页多个表单）；
4. 页面 `Resources`（字体/图片）原样克隆，保证矢量无损。

注意事项：
- 注释边框本身（红框）不会被绘制到输出中——注释不属于页面内容流，输出更干净；
- 暂不处理带 `/Rotate` 旋转的页面（按未旋转坐标裁剪）。

## License

MIT
