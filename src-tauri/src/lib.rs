//! Tauri 应用层：把前端请求转交给 pdf-crop-core 处理。

use std::path::PathBuf;

use pdf_crop_core::{crop_pdf, parse_page_range, CoreError, CropOptions, CropSource, Layout};
use serde::Serialize;

/// 返回给前端的结果
#[derive(Serialize)]
struct CropResult {
    regions: usize,
    pages: usize,
    output: String,
}

/// 裁剪主命令：由前端 invoke 调用
///
/// Args:
///     input: 源 PDF 绝对路径
///     output_dir: 输出目录；空串 = 源文件同目录
///     mode: "annotation"（矩形注释）或 "margin"（页边距）
///     layout: "per_region"（独立分页）或 "flow"（拼版排版）
///     expand_mode: 扩展模式（同页注释取并集）
///     page_range: 页码范围字符串 "1-3,5"，空 = 全部
#[tauri::command]
fn crop_pdf_cmd(
    input: String,
    output_dir: String,
    mode: String,
    layout: String,
    expand_mode: bool,
    page_range: String,
) -> Result<CropResult, String> {
    let input_path = PathBuf::from(&input);
    if !input_path.exists() {
        return Err("源 PDF 文件不存在".into());
    }

    // 输出文件路径：输出目录 + 时间戳文件名
    let dir = if output_dir.trim().is_empty() {
        input_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        let d = PathBuf::from(output_dir.trim());
        std::fs::create_dir_all(&d).map_err(|e| format!("创建输出目录失败: {e}"))?;
        d
    };
    let stamp = chrono_like_stamp();
    let stem = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let output_path = dir.join(format!("{stem}_裁剪_{stamp}.pdf"));

    // 组装核心选项
    let source = match mode.as_str() {
        "margin" => CropSource::PageMargin { margin_pt: 36.0 },
        _ => CropSource::RectAnnotation { expand_mode },
    };
    let layout = match layout.as_str() {
        "flow" => Layout::Flow { margin: 36.0, gap: 12.0 },
        _ => Layout::PerRegion,
    };

    // 页码范围需要先知道总页数：交给核心库在收集阶段过滤，
    // 这里先加载文档拿页数（与核心库行为一致）
    let options = CropOptions {
        source,
        layout,
        page_numbers: vec![], // 先占位，下面填充
    };

    // 加载源文档解析页码范围
    let doc = lopdf::Document::load(&input_path).map_err(|e| e.to_string())?;
    let total = doc.get_pages().len() as u32;
    drop(doc);
    let mut options = options;
    options.page_numbers = parse_page_range(&page_range, total).map_err(err_to_string)?;

    let outcome = crop_pdf(&input_path, &output_path, &options).map_err(err_to_string)?;
    Ok(CropResult {
        regions: outcome.regions,
        pages: outcome.pages,
        output: outcome.output,
    })
}

/// 生成 YYYYMMDD_HHMMSS 时间戳（不引入 chrono，减少依赖体积）
fn chrono_like_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 简单的 UTC+8 换算（工具面向中国用户）—— 精确时区对文件名不重要
    let secs = secs + 8 * 3600;
    let days = secs / 86400;
    let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    // 从 1970-01-01 起的日期算法（简化民用历法换算）
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}{mo:02}{d:02}_{h:02}{m:02}{s:02}")
}

/// Unix 天数 -> (年, 月, 日)；Howard Hinnant 的经典算法
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 应用启动入口
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![crop_pdf_cmd])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
