//! PDF 矩形注释裁剪核心逻辑（纯 Rust 实现，与 PyMuPDF 版功能对齐）。
//!
//! 裁剪原理（与 PyMuPDF 的 `show_pdf_page(clip=...)` 相同的 PDF 标准技法）：
//! 把源页面的内容流包装成 Form XObject，BBox 设为裁剪区域，
//! 再以 `q W w cm /Form Do Q` 的方式绘制到新页面上。
//! 全程矢量操作，文字仍是文字，不会栅格化。
//!
//! 注意：注释（Annotation）本身不属于页面内容流，因此输出中
//! 不包含注释边框的视觉呈现（红框不会被画出来），裁到的是框内内容。

use std::path::Path;

use lopdf::{dictionary, Document as PdfDocument, Object, ObjectId};
use thiserror::Error;

// ============================================================
// 错误类型
// ============================================================
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("lopdf 错误: {0}")]
    Lopdf(#[from] lopdf::Error),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

impl CoreError {
    fn msg<S: Into<String>>(s: S) -> Self {
        CoreError::Message(s.into())
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

// ============================================================
// 公共数据结构与选项
// ============================================================
/// 裁剪区域：来自第 page_number 页（1 基）的矩形 [x0, y0, x1, y1]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropRegion {
    pub page_number: u32,
    pub rect: [f64; 4],
}

/// 排版方式
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layout {
    /// 独立分页：每个区域单独一页，页面尺寸 = 区域尺寸
    PerRegion,
    /// 拼版排版：像 Word 插图一样流式排列到 A4 页面
    Flow { margin: f64, gap: f64 },
}

/// 裁剪来源类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CropSource {
    /// 矩形注释（Square Annotation）；expand_mode = 本页注释取并集整块裁剪
    RectAnnotation { expand_mode: bool },
    /// 页边距裁剪：每页去掉四周 margin_pt 留白
    PageMargin { margin_pt: f64 },
}

/// 裁剪任务选项
#[derive(Debug, Clone)]
pub struct CropOptions {
    pub source: CropSource,
    pub layout: Layout,
    /// 1 基页码白名单；空 Vec 表示全部页面
    pub page_numbers: Vec<u32>,
}

/// 裁剪结果统计
#[derive(Debug, Clone)]
pub struct CropOutcome {
    pub regions: usize,
    pub pages: usize,
    pub output: String,
}

// ============================================================
// 工具函数
// ============================================================
/// 解析一层间接引用（Object::Reference -> 实际对象）
fn resolve<'a>(doc: &'a PdfDocument, obj: &'a Object) -> Result<&'a Object> {
    match obj {
        Object::Reference(id) => Ok(doc.get_object(*id)?),
        _ => Ok(obj),
    }
}

/// 把 Object 强转为 Dictionary
fn as_dict<'a>(obj: &'a Object, what: &str) -> Result<&'a lopdf::Dictionary> {
    match obj {
        Object::Dictionary(d) => Ok(d),
        _ => Err(CoreError::msg(format!("{what} 不是 Dictionary 对象"))),
    }
}

/// 从 Object 数组解析 [x0, y0, x1, y1]（兼容 Integer / Real）
fn parse_rect(obj: &Object) -> Result<[f64; 4]> {
    let items = match obj {
        Object::Array(items) => items,
        _ => return Err(CoreError::msg("Rect 不是数组")),
    };
    if items.len() != 4 {
        return Err(CoreError::msg("Rect 数组长度不为 4"));
    }
    let mut out = [0.0f64; 4];
    for (i, item) in items.iter().enumerate() {
        out[i] = match item {
            Object::Integer(n) => *n as f64,
            Object::Real(r) => *r as f64,
            _ => return Err(CoreError::msg("Rect 含非数字元素")),
        };
    }
    Ok(out)
}

/// 读取页面 MediaBox（缺省 [0 0 612 792]）
fn page_mediabox(doc: &PdfDocument, page_id: ObjectId) -> Result<[f64; 4]> {
    let page = doc.get_object(page_id)?;
    let dict = as_dict(page, "Page")?;
    match dict.get(b"MediaBox") {
        Ok(mb) => parse_rect(resolve(doc, mb)?),
        Err(_) => Ok([0.0, 0.0, 612.0, 792.0]),
    }
}

/// 归一化矩形：保证 x0<=x1, y0<=y1
fn normalize(rect: [f64; 4]) -> [f64; 4] {
    [
        rect[0].min(rect[2]),
        rect[1].min(rect[3]),
        rect[0].max(rect[2]),
        rect[1].max(rect[3]),
    ]
}

/// 两矩形求交集，无交集返回 None
fn intersect(a: [f64; 4], b: [f64; 4]) -> Option<[f64; 4]> {
    let r = [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])];
    if r[0] < r[2] && r[1] < r[3] {
        Some(r)
    } else {
        None
    }
}

/// 解析页码范围字符串（1 基），支持 "1-3,5"；空串 = 全部页面。
/// 超出 total_pages 的页码自动忽略。
pub fn parse_page_range(range: &str, total_pages: u32) -> Result<Vec<u32>> {
    let text = range.trim();
    if text.is_empty() {
        return Ok((1..=total_pages).collect());
    }
    let mut pages = std::collections::BTreeSet::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((s, e)) = part.split_once('-') {
            let s: u32 = s
                .trim()
                .parse()
                .map_err(|_| CoreError::msg(format!("页码不合法: {part}")))?;
            let e: u32 = e
                .trim()
                .parse()
                .map_err(|_| CoreError::msg(format!("页码不合法: {part}")))?;
            if s < 1 || e < s {
                return Err(CoreError::msg(format!("页码范围不合法: {part}")));
            }
            for p in s..=e.min(total_pages) {
                pages.insert(p);
            }
        } else {
            let n: u32 = part
                .parse()
                .map_err(|_| CoreError::msg(format!("页码不合法: {part}")))?;
            if n < 1 {
                return Err(CoreError::msg(format!("页码必须从 1 开始: {part}")));
            }
            if n <= total_pages {
                pages.insert(n);
            }
        }
    }
    Ok(pages.into_iter().collect())
}

// ============================================================
// 第一步：收集裁剪区域
// ============================================================
/// 按选项遍历文档，收集全部裁剪区域（顺序：页码 -> 页面内注释顺序）
pub fn collect_regions(doc: &PdfDocument, options: &CropOptions) -> Result<Vec<CropRegion>> {
    let mut regions = Vec::new();
    for (page_no, page_id) in doc.get_pages() {
        if !options.page_numbers.is_empty() && !options.page_numbers.contains(&page_no) {
            continue;
        }
        match options.source {
            CropSource::PageMargin { margin_pt } => {
                let mb = page_mediabox(doc, page_id)?;
                // 四边各收缩 margin_pt
                let cropped = [
                    mb[0] + margin_pt,
                    mb[1] + margin_pt,
                    mb[2] - margin_pt,
                    mb[3] - margin_pt,
                ];
                if let Some(r) = intersect(cropped, mb) {
                    regions.push(CropRegion { page_number: page_no, rect: r });
                }
            }
            CropSource::RectAnnotation { expand_mode } => {
                let annots = page_rect_annotations(doc, page_id)?;
                if annots.is_empty() {
                    continue;
                }
                if expand_mode {
                    // 扩展模式：本页所有注释取并集（外接矩形）整块裁剪
                    let mut union = annots[0];
                    for r in &annots[1..] {
                        union = [
                            union[0].min(r[0]),
                            union[1].min(r[1]),
                            union[2].max(r[2]),
                            union[3].max(r[3]),
                        ];
                    }
                    regions.push(CropRegion { page_number: page_no, rect: union });
                } else {
                    regions.extend(
                        annots.into_iter().map(|r| CropRegion { page_number: page_no, rect: r }),
                    );
                }
            }
        }
    }
    Ok(regions)
}

/// 读取单个页面上所有矩形注释（Subtype=/Square）的 Rect
fn page_rect_annotations(doc: &PdfDocument, page_id: ObjectId) -> Result<Vec<[f64; 4]>> {
    let page = doc.get_object(page_id)?;
    let dict = as_dict(page, "Page")?;
    let annots_obj = match dict.get(b"Annots") {
        Ok(a) => resolve(doc, a)?,
        Err(_) => return Ok(Vec::new()),
    };
    let annots = match annots_obj {
        Object::Array(items) => items,
        _ => return Ok(Vec::new()),
    };

    let mut rects = Vec::new();
    for item in annots {
        let annot = resolve(doc, item)?;
        let adict = match annot {
            Object::Dictionary(d) => d,
            _ => continue,
        };
        let is_square = matches!(adict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Square");
        if !is_square {
            continue;
        }
        // 跳过隐藏注释（/F 标志位第 2 位 = Hidden）
        if let Ok(Object::Integer(flags)) = adict.get(b"F") {
            if flags & 2 != 0 {
                continue;
            }
        }
        if let Ok(rect_obj) = adict.get(b"Rect") {
            let r = normalize(parse_rect(resolve(doc, rect_obj)?)?);
            if r[2] > r[0] && r[3] > r[1] {
                rects.push(r);
            }
        }
    }
    Ok(rects)
}

// ============================================================
// 第二步：Form XObject（矢量裁剪的关键）
// ============================================================
/// 拼接页面所有内容流（已解码）
/// 读取流的解码后字节。
/// 注意：lopdf 的 `decompressed_content()` 在流没有 /Filter 键时会报错，
/// 因此这里判断：有 /Filter 才解压，否则直接取明文内容（get_plain_content）。
fn stream_bytes(s: &lopdf::Stream) -> Result<Vec<u8>> {
    if s.dict.get(b"Filter").is_ok() {
        Ok(s.decompressed_content()?)
    } else {
        Ok(s.get_plain_content()?)
    }
}

fn page_content_bytes(doc: &PdfDocument, page_id: ObjectId) -> Result<Vec<u8>> {
    let page = doc.get_object(page_id)?;
    let dict = as_dict(page, "Page")?;
    let contents = match dict.get(b"Contents") {
        Ok(c) => resolve(doc, c)?,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    match contents {
        Object::Stream(s) => out.extend_from_slice(&stream_bytes(s)?),
        Object::Array(items) => {
            for item in items {
                if let Object::Stream(s) = resolve(doc, item)? {
                    out.extend_from_slice(&stream_bytes(s)?);
                    out.extend_from_slice(b"\n");
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// 把源页面的内容（裁剪到 crop 区域）作为 Form XObject 加入输出文档。
///
/// BBox = crop（限定可见范围），Matrix = [1 0 0 1 -x0 -y0]（把区域原点平移到 0,0）。
/// 返回新表单对象在输出文档中的 ObjectId。
fn add_form_xobject(
    out_doc: &mut PdfDocument,
    src: &PdfDocument,
    page_id: ObjectId,
    crop: [f64; 4],
) -> Result<ObjectId> {
    let crop = normalize(crop);
    let content = page_content_bytes(src, page_id)?;

    // 直接克隆源页面的 Resources（字体/图片引用全部保留 -> 矢量无损）
    let page = src.get_object(page_id)?;
    let pdict = as_dict(page, "Page")?;
    let resources = match pdict.get(b"Resources") {
        Ok(r) => resolve(src, r)?.clone(),
        Err(_) => Object::Dictionary(dictionary! {}),
    };

    let dict = dictionary! {
        "Type" => Object::Name(b"XObject".to_vec()),
        "Subtype" => Object::Name(b"Form".to_vec()),
        "FormType" => Object::Integer(1),
        "BBox" => Object::Array(vec![
            Object::Real(crop[0] as f32), Object::Real(crop[1] as f32),
            Object::Real(crop[2] as f32), Object::Real(crop[3] as f32),
        ]),
        "Matrix" => Object::Array(vec![
            Object::Integer(1), Object::Integer(0),
            Object::Integer(0), Object::Integer(1),
            Object::Real(-crop[0] as f32), Object::Real(-crop[1] as f32),
        ]),
        "Resources" => resources,
    };
    Ok(out_doc.add_object(lopdf::Stream::new(dict, content)))
}

// ============================================================
// 第三步：页面累积器（支持一页放多个区域 —— 拼版模式的关键）
// ============================================================
/// 输出页面的累积状态：内容绘制指令 + 该页引用到的表单
struct PageAcc {
    ops: Vec<String>,
    forms: Vec<(String, ObjectId)>, // (表单名, ObjectId)
}

impl PageAcc {
    fn new() -> Self {
        PageAcc { ops: Vec::new(), forms: Vec::new() }
    }

    /// 把累积内容落成真正的 Page 对象，返回页面对象 id
    fn flush(self, out_doc: &mut PdfDocument, page_w: f64, page_h: f64) -> ObjectId {
        let content = self.ops.join("\n");
        let content_dict = dictionary! {
            "Length" => Object::Integer(content.len() as i64),
        };
        let content_id = out_doc.add_object(lopdf::Stream::new(content_dict, content.into_bytes()));

        let mut xobj = lopdf::Dictionary::new();
        for (name, id) in &self.forms {
            xobj.set(name.as_bytes(), Object::Reference(*id));
        }
        let page_dict = dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "MediaBox" => Object::Array(vec![
                Object::Real(0.0), Object::Real(0.0),
                Object::Real(page_w as f32), Object::Real(page_h as f32),
            ]),
            "Resources" => dictionary! {
                "XObject" => Object::Dictionary(xobj),
                "ProcSet" => Object::Array(vec![
                    Object::Name(b"PDF".to_vec()),
                    Object::Name(b"Text".to_vec()),
                    Object::Name(b"ImageB".to_vec()),
                    Object::Name(b"ImageC".to_vec()),
                    Object::Name(b"ImageI".to_vec()),
                ]),
            },
            "Contents" => Object::Reference(content_id),
        };
        out_doc.add_object(page_dict)
    }
}

/// 数值格式化：整数不带小数点
fn fmt(v: f64) -> String {
    if (v - v.round()).abs() < 1e-6 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.4}")
    }
}

/// 生成绘制某个表单的指令：
/// q -> cm(缩放到 disp_w x disp_h 并平移到 x,y) -> Do -> Q
fn draw_op(form_name: &str, x: f64, y: f64, w: f64, h: f64) -> String {
    format!("q\n{} 0 0 {} {} {} cm\n/{} Do\nQ", fmt(w), fmt(h), fmt(x), fmt(y), form_name)
}

/// 重建输出文档结构（Catalog + Pages 树 + Trailer）
fn finalize_document(out_doc: &mut PdfDocument, kids: Vec<ObjectId>) {
    let pages_dict = dictionary! {
        "Type" => Object::Name(b"Pages".to_vec()),
        "Count" => Object::Integer(kids.len() as i64),
        "Kids" => Object::Array(kids.into_iter().map(Object::Reference).collect()),
    };
    let pages_id = out_doc.add_object(pages_dict);
    let catalog_dict = dictionary! {
        "Type" => Object::Name(b"Catalog".to_vec()),
        "Pages" => Object::Reference(pages_id),
    };
    let catalog_id = out_doc.add_object(catalog_dict);
    out_doc.trailer.set("Root", Object::Reference(catalog_id));
}

// ============================================================
// 第四步：主入口
// ============================================================
/// 主流程：读取源 PDF -> 收集区域 -> 按排版方式生成新 PDF
pub fn crop_pdf(input: &Path, output: &Path, options: &CropOptions) -> Result<CropOutcome> {
    let src = PdfDocument::load(input)
        .map_err(|e| CoreError::msg(format!("无法打开 PDF: {e}")))?;

    let regions = collect_regions(&src, options)?;
    if regions.is_empty() {
        return Err(CoreError::msg(
            "未找到任何可裁剪的区域（没有匹配的矩形注释，或页码范围内无内容）",
        ));
    }

    // 把每个裁剪区域「物化」为输出文档里的一个 Form XObject，
    // 同时记录绘制指令所需的区域尺寸
    struct Prepared {
        form_id: ObjectId,
        region_w: f64,
        region_h: f64,
    }
    let mut prepared = Vec::with_capacity(regions.len());
    let mut out_doc = PdfDocument::new();
    for region in &regions {
        let page_id = src
            .get_pages()
            .get(&region.page_number)
            .copied()
            .ok_or_else(|| CoreError::msg(format!("页码越界: {}", region.page_number)))?;
        let rect = intersect(region.rect, page_mediabox(&src, page_id)?)
            .ok_or_else(|| CoreError::msg("裁剪区域与页面无交集"))?;
        let form_id = add_form_xobject(&mut out_doc, &src, page_id, rect)?;
        prepared.push(Prepared {
            form_id,
            region_w: rect[2] - rect[0],
            region_h: rect[3] - rect[1],
        });
    }

    let mut kids: Vec<ObjectId> = Vec::new();
    let mut n_forms_used = 0usize;

    match options.layout {
        // ---------- 模式一：独立分页，页面尺寸 = 区域尺寸 ----------
        Layout::PerRegion => {
            for (i, p) in prepared.iter().enumerate() {
                let mut acc = PageAcc::new();
                let name = format!("Form{i}");
                acc.forms.push((name.clone(), p.form_id));
                acc.ops.push(draw_op(&name, 0.0, 0.0, p.region_w, p.region_h));
                kids.push(acc.flush(&mut out_doc, p.region_w, p.region_h));
                n_forms_used += 1;
            }
        }
        // ---------- 模式二：拼版排版（Word 式流式排列到 A4） ----------
        Layout::Flow { margin, gap } => {
            const PAGE_W: f64 = 595.0; // A4 纵向
            const PAGE_H: f64 = 842.0;
            let content_w = PAGE_W - 2.0 * margin;
            let content_h = PAGE_H - 2.0 * margin;
            if content_w <= 0.0 || content_h <= 0.0 {
                return Err(CoreError::msg("页边距过大，版心尺寸非法"));
            }

            let mut acc: Option<PageAcc> = None;
            let mut cursor_y = 0.0f64;

            for p in &prepared {
                // 等比缩放到版心内（小图不放大；整页都放不下的缩到一页高）
                let mut scale = (content_w / p.region_w).min(1.0);
                if p.region_h * scale > content_h {
                    scale *= content_h / (p.region_h * scale);
                }
                let disp_w = p.region_w * scale;
                let disp_h = p.region_h * scale;

                // 当前页放不下 -> 落盘旧页、另起新页
                let need_new_page = match &acc {
                    None => true,
                    Some(_) => cursor_y + disp_h > content_h,
                };
                if need_new_page {
                    if let Some(old) = acc.take() {
                        kids.push(old.flush(&mut out_doc, PAGE_W, PAGE_H));
                    }
                    acc = Some(PageAcc::new());
                    cursor_y = 0.0;
                }

                let page_acc = acc.as_mut().unwrap();
                let name = format!("Form{n_forms_used}");
                n_forms_used += 1;
                page_acc.forms.push((name.clone(), p.form_id));
                // 水平居中，垂直方向排在 cursor_y 处
                let x = margin + (content_w - disp_w) / 2.0;
                page_acc.ops.push(draw_op(&name, x, margin + cursor_y, disp_w, disp_h));
                cursor_y += disp_h + gap;
            }
            // 落盘最后一页
            if let Some(last) = acc.take() {
                kids.push(last.flush(&mut out_doc, PAGE_W, PAGE_H));
            }
        }
    }

    if kids.is_empty() {
        return Err(CoreError::msg("没有可写入的页面"));
    }
    let pages = kids.len();
    finalize_document(&mut out_doc, kids);
    out_doc.save(output)?;

    Ok(CropOutcome {
        regions: n_forms_used,
        pages,
        output: output.to_string_lossy().into_owned(),
    })
}

// ============================================================
// 单元测试（CI 上会执行）
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 用 lopdf 现场构造一个测试 PDF：
    /// 3 页，前 2 页各有 2 个 Square 注释，第 3 页没有注释
    fn make_sample_pdf(path: &Path) {
        let mut doc = PdfDocument::new();
        let mut kids = Vec::new();

        for i in 1..=3u32 {
            let content = format!(
                "BT /F1 12 Tf 72 100 Td (Page {i} content) Tj ET\n"
            );
            let content_dict = dictionary! { "Length" => Object::Integer(content.len() as i64) };
            let content_id = doc.add_object(lopdf::Stream::new(content_dict, content.into_bytes()));

            // 页面资源里放一个字体引用，验证资源克隆
            let font_dict = dictionary! {
                "Type" => Object::Name(b"Font".to_vec()),
                "Subtype" => Object::Name(b"Type1".to_vec()),
                "BaseFont" => Object::Name(b"Helvetica".to_vec()),
            };
            let font_id = doc.add_object(font_dict);

            let mut page = dictionary! {
                "Type" => Object::Name(b"Page".to_vec()),
                "MediaBox" => Object::Array(vec![
                    Object::Integer(0), Object::Integer(0),
                    Object::Integer(595), Object::Integer(842),
                ]),
                "Resources" => dictionary! {
                    "Font" => dictionary! { "F1" => Object::Reference(font_id) },
                },
                "Contents" => Object::Reference(content_id),
            };

            if i <= 2 {
                // 两个矩形注释：100,150,300,250 和 320,300,520,420
                let mk_annot = |rect: [i64; 4]| {
                    dictionary! {
                        "Type" => Object::Name(b"Annot".to_vec()),
                        "Subtype" => Object::Name(b"Square".to_vec()),
                        "Rect" => Object::Array(
                            rect.iter().map(|v| Object::Integer(*v)).collect(),
                        ),
                        "F" => Object::Integer(4), // Print 标志，非隐藏
                    }
                };
                let a1 = doc.add_object(mk_annot([100, 150, 300, 250]));
                let a2 = doc.add_object(mk_annot([320, 300, 520, 420]));
                page.set(
                    b"Annots",
                    Object::Array(vec![Object::Reference(a1), Object::Reference(a2)]),
                );
            }

            let page_id = doc.add_object(page);
            kids.push(page_id);
        }

        finalize_document(&mut doc, kids);
        doc.save(path).unwrap();
    }

    fn tmp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // 每个测试用独立子目录：cargo test 默认多线程并行，
        // 共享目录会导致 make_sample_pdf 互相覆写 sample.pdf 引发竞态
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("pdf-crop-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn test_parse_page_range() {
        assert_eq!(parse_page_range("", 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_page_range("2", 3).unwrap(), vec![2]);
        assert_eq!(parse_page_range("1-3", 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_page_range("1,3", 3).unwrap(), vec![1, 3]);
        assert_eq!(parse_page_range("2-9", 3).unwrap(), vec![2, 3]); // 超界截断
        assert!(parse_page_range("abc", 3).is_err());
        assert!(parse_page_range("0", 3).is_err());
    }

    #[test]
    fn test_collect_rect_annotations() {
        let dir = tmp_dir();
        let src = dir.join("sample.pdf");
        make_sample_pdf(&src);

        let doc = PdfDocument::load(&src).unwrap();
        let opts = CropOptions {
            source: CropSource::RectAnnotation { expand_mode: false },
            layout: Layout::PerRegion,
            page_numbers: vec![],
        };
        let regions = collect_regions(&doc, &opts).unwrap();
        assert_eq!(regions.len(), 4, "前 2 页各 2 个注释");
        assert_eq!(regions[0].page_number, 1);
        assert_eq!(regions[0].rect, [100.0, 150.0, 300.0, 250.0]);
    }

    #[test]
    fn test_crop_per_region() {
        let dir = tmp_dir();
        let src = dir.join("sample.pdf");
        let out = dir.join("out_per_region.pdf");
        make_sample_pdf(&src);

        let outcome = crop_pdf(
            &src,
            &out,
            &CropOptions {
                source: CropSource::RectAnnotation { expand_mode: false },
                layout: Layout::PerRegion,
                page_numbers: vec![],
            },
        )
        .unwrap();
        assert_eq!(outcome.regions, 4);
        assert_eq!(outcome.pages, 4);

        // 校验输出：4 页，页面尺寸 = 注释区域尺寸
        let out_doc = PdfDocument::load(&out).unwrap();
        assert_eq!(out_doc.get_pages().len(), 4);
        let pages = out_doc.get_pages();
        let first = pages.get(&1).unwrap();
        let mb = page_mediabox(&out_doc, *first).unwrap();
        assert_eq!((mb[2], mb[3]), (200.0, 100.0));
    }

    #[test]
    fn test_crop_flow_layout() {
        let dir = tmp_dir();
        let src = dir.join("sample.pdf");
        let out = dir.join("out_flow.pdf");
        make_sample_pdf(&src);

        let outcome = crop_pdf(
            &src,
            &out,
            &CropOptions {
                source: CropSource::RectAnnotation { expand_mode: false },
                layout: Layout::Flow { margin: 36.0, gap: 12.0 },
                page_numbers: vec![],
            },
        )
        .unwrap();

        // 4 个区域（最大 220x122pt）远小于 A4 版心高 770pt -> 应排进 1 页
        assert_eq!(outcome.regions, 4);
        assert_eq!(outcome.pages, 1, "4 个小区域应排进 1 页 A4");

        let out_doc = PdfDocument::load(&out).unwrap();
        assert_eq!(out_doc.get_pages().len(), 1);
        let pages = out_doc.get_pages();
        let first = pages.get(&1).unwrap();
        let mb = page_mediabox(&out_doc, *first).unwrap();
        assert_eq!((mb[2], mb[3]), (595.0, 842.0), "拼版页面必须是 A4");
    }

    #[test]
    fn test_page_range_filter() {
        let dir = tmp_dir();
        let src = dir.join("sample.pdf");
        let out = dir.join("out_filtered.pdf");
        make_sample_pdf(&src);

        // 只处理第 3 页 -> 没有注释，应报错
        let r = crop_pdf(
            &src,
            &out,
            &CropOptions {
                source: CropSource::RectAnnotation { expand_mode: false },
                layout: Layout::PerRegion,
                page_numbers: vec![3],
            },
        );
        assert!(r.is_err());

        // 只处理第 1 页 -> 2 个区域
        let outcome = crop_pdf(
            &src,
            &out,
            &CropOptions {
                source: CropSource::RectAnnotation { expand_mode: false },
                layout: Layout::PerRegion,
                page_numbers: vec![1],
            },
        )
        .unwrap();
        assert_eq!(outcome.regions, 2);
    }

    #[test]
    fn test_expand_mode() {
        let dir = tmp_dir();
        let src = dir.join("sample.pdf");
        let out = dir.join("out_expand.pdf");
        make_sample_pdf(&src);

        // 每页注释并集：(100,150)-(520,420) -> 420 x 270
        let outcome = crop_pdf(
            &src,
            &out,
            &CropOptions {
                source: CropSource::RectAnnotation { expand_mode: true },
                layout: Layout::PerRegion,
                page_numbers: vec![],
            },
        )
        .unwrap();
        assert_eq!(outcome.regions, 2); // 每页 1 个并集区域
        assert_eq!(outcome.pages, 2);

        let out_doc = PdfDocument::load(&out).unwrap();
        let pages = out_doc.get_pages();
        let first = pages.get(&1).unwrap();
        let mb = page_mediabox(&out_doc, *first).unwrap();
        assert_eq!((mb[2], mb[3]), (420.0, 270.0));
    }

    #[test]
    fn test_margin_mode() {
        let dir = tmp_dir();
        let src = dir.join("sample.pdf");
        let out = dir.join("out_margin.pdf");
        make_sample_pdf(&src);

        // 页边距模式：3 页每页裁掉 36pt 边距 -> 523 x 770
        let outcome = crop_pdf(
            &src,
            &out,
            &CropOptions {
                source: CropSource::PageMargin { margin_pt: 36.0 },
                layout: Layout::PerRegion,
                page_numbers: vec![],
            },
        )
        .unwrap();
        assert_eq!(outcome.regions, 3);

        let out_doc = PdfDocument::load(&out).unwrap();
        let pages = out_doc.get_pages();
        let first = pages.get(&1).unwrap();
        let mb = page_mediabox(&out_doc, *first).unwrap();
        assert_eq!((mb[2], mb[3]), (523.0, 770.0));
    }
}
