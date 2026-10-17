// 前端逻辑：调用 Tauri 全局 API（withGlobalTauri 已启用，无需打包 npm 依赖）
const { invoke } = window.__TAURI__.core;
const { open } = window.__TAURI__.dialog;

const $ = (id) => document.getElementById(id);

// ---- 选择输入文件 ----
$("pickBtn").addEventListener("click", async () => {
  const path = await open({
    multiple: false,
    filters: [{ name: "PDF 文件", extensions: ["pdf"] }],
  });
  if (path) $("inputFile").value = path;
});

// ---- 提交裁剪 ----
$("submitBtn").addEventListener("click", async () => {
  const input = $("inputFile").value.trim();
  const result = $("result");
  const btn = $("submitBtn");

  if (!input) {
    show("请先选择 PDF 文件", false);
    return;
  }

  const mode = document.querySelector('input[name="mode"]:checked').value;
  const layout = document.querySelector('input[name="layout"]:checked').value;

  btn.disabled = true;
  show("处理中，请稍候……", true, "ok");

  try {
    const r = await invoke("crop_pdf_cmd", {
      input,
      outputDir: $("outputDir").value.trim(),
      mode,
      layout,
      expandMode: $("expandMode").checked,
      pageRange: $("pageRange").value.trim(),
    });
    show(
      `✅ 裁剪完成！\n` +
        `- 裁剪区域数：${r.regions}\n` +
        `- 输出页数：${r.pages}\n` +
        `- 输出文件：${r.output}`,
      true,
      "ok"
    );
  } catch (e) {
    show("❌ " + e, false);
  } finally {
    btn.disabled = false;
  }
});

function show(text, ok, cls) {
  const el = $("result");
  el.textContent = text;
  el.className = ok ? "ok" : "err";
  if (cls) el.className = cls;
}
