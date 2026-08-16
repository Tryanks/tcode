use crate::{FileChange, FileChangeKind, ItemContent, ThreadItem, TokenUsage};

pub(crate) fn token_usage(
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    used_tokens: Option<u64>,
) -> TokenUsage {
    TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        used_tokens,
        ..TokenUsage::default()
    }
}

pub(crate) fn thread_item(id: impl Into<String>, content: ItemContent) -> ThreadItem {
    ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content,
    }
}

pub(crate) fn file_change(
    path: impl Into<String>,
    kind: FileChangeKind,
    diff: Option<String>,
) -> FileChange {
    FileChange {
        path: path.into(),
        kind,
        diff,
    }
}
