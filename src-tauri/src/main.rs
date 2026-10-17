// Tauri 桌面应用入口（Windows 下 release 模式隐藏控制台窗口）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    pdf_crop_lib::run()
}
