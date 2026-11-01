use crate::ToolName;

// Tool results are model inputs, but generated images are not read operations.
// Only recognize explicit read/capture tools; an image block alone is insufficient.
pub(crate) fn is_read_tool(name: &str) -> bool {
    let tool = ToolName::parse(name).tool;
    let tool = tool.rsplit('.').next().unwrap_or(tool);
    matches!(
        tool.to_ascii_lowercase().as_str(),
        "read"
            | "read_file"
            | "read_image"
            | "view_image"
            | "image_view"
            | "viewimage"
            | "screenshot"
            | "take_screenshot"
            | "capture_screenshot"
            | "browser_take_screenshot"
            | "preview_screenshot"
            | "read_media_file"
            | "observe_ui"
            | "inspect_ui"
            | "expand_ui"
            | "search_ui"
            | "act_ui"
            | "wait_for"
    )
}
