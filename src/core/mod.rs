pub mod types;
pub mod alias;
pub mod fallback;

/// cr-411 P2: 提取 `<think>...</think>` 块. 返回 (visible, reasoning).
/// MiniMax 等后端把 thinking 文字混在 content 里, 格式: "<think>reasoning</think>\n\nactual"
pub fn extract_thinking(text: &str) -> (String, Option<String>) {
    let trimmed = text.trim();
    // 检测整段是 <think>...</think> (含跨多块) - 简化: 只处理一个 think 块 + 前缀/后缀
    if let Some(start) = trimmed.find("<think>") {
        if let Some(end_rel) = trimmed[start..].find("</think>") {
            let end = start + end_rel + "</think>".len();
            let thinking = trimmed[start + "<think>".len()..start + end_rel].trim().to_string();
            let visible = format!("{}{}", &trimmed[..start], &trimmed[end..]).trim().to_string();
            return (visible, Some(thinking));
        }
    }
    (text.to_string(), None)
}
