use std::path::PathBuf;

use tcode_core::session::{EntryContent, Timeline};
use tcode_services::store::SessionStore;

const SESSION_ID: &str = "vt-markdown-demo";
const USER_MESSAGE: &str = "Show me the markdown demo (100%)";
const ASSISTANT_MARKDOWN: &str = r#"# H1

## H2

This paragraph has **bold**, *italic*, ~~strikethrough~~, and `inline code`.

```rust
fn main() {
    let language = "Rust";
    // A five-line syntax-highlighting sample.
    let message = format!("Hello from {language}!");
    println!("{message}");
}
```

```typescript
const count: number = 3;
interface Demo {
  title: string;
  enabled: boolean;
}
const demo: Demo = { title: "TypeScript", enabled: true };
```

```python
def greet(name: str) -> str:
    message = f"Hello, {name}!"
    return message

print(greet("Python"))
```

```go
package main
func main() {
    message := "Hello from Go"
    println(message)
}
```

```toml
[demo]
title = "TOML sample"
enabled = true
count = 3
```

```kotlin
fun main() {
    val language: String = "Kotlin"
    println("Hello from $language")
}
```

```text
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

| Left | Center | Right |
| :--- | :----: | ----: |
| alpha | beta | 100 |
| gamma | delta | 200 |

| Name | Status | Owner | Description | Count | Notes |
| :--- | :----: | :--- | :---------- | ----: | :---- |
| Short | Ready | UI | This deliberately long table cell contains about sixty characters total. | 12 | wraps |
| Longer component name | Pending | Visual QA | Brief | 3 | compact |

- [x] Render headings
- [ ] Inspect every pixel

1. Ordered parent
   - Unordered child
     1. Nested ordered child
2. Second ordered item

- Unordered parent
  - Nested bullet
    1. Ordered grandchild

> First line of the blockquote.
> Second line of the blockquote.

![demo screenshot](https://example.com/image.png)

Visit [Example](https://example.com) or the bare URL https://tcode.dev for more.

---

This paragraph has a soft
line break, followed by a hard break.  
This begins after the hard break."#;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.claude/fixtures/vt")
}

#[test]
fn visual_test_fixture_replays_through_session_store() {
    let store = SessionStore::open_at(fixture_root()).expect("fixture store should open");
    let index = store.load_index();
    assert_eq!(index.len(), 1);
    assert_eq!(index[0].id, SESSION_ID);
    assert_eq!(index[0].cwd, PathBuf::from("/tmp/tcode-vt-project"));

    let events = store.read_events(SESSION_ID);
    assert_eq!(
        events.len(),
        7,
        "every fixture JSONL line should deserialize"
    );
    let timeline = Timeline::fold_events(events);

    assert!(matches!(
        &timeline.entries[0].content,
        EntryContent::User { text, .. } if text == USER_MESSAGE
    ));
    assert!(timeline.entries.iter().any(|entry| matches!(
        &entry.content,
        EntryContent::Assistant { text } if text == ASSISTANT_MARKDOWN
    )));
    assert!(timeline.entries.iter().any(|entry| matches!(
        &entry.content,
        EntryContent::Reasoning { text }
            if text == "I’ll assemble a compact markdown showcase and verify each requested construct."
    )));
    assert_eq!(
        timeline
            .proposed_plan
            .as_ref()
            .map(|plan| plan.markdown.as_str()),
        Some(
            "## Demo follow-up\n\n1. Review the rendered markdown.\n2. Capture the visual baseline."
        )
    );
    assert_eq!(timeline.turns.len(), 1);
    assert_eq!(timeline.turns[0].changes.as_ref().unwrap().changes.len(), 1);
    assert!(!timeline.turn_running);
}
