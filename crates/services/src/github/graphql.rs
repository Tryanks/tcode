use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Document {
    pub query: String,
    pub variables: BTreeMap<String, Value>,
}
pub type Variables = BTreeMap<String, (String, Value)>;

pub struct AliasItem {
    pub key: usize,
    pub variables: Variables,
}

/// Callers supply trusted schema text; values are delivered only in the variables map.
pub fn aliases(
    operation: &str,
    name: &str,
    prefix: &str,
    items: &[AliasItem],
    shared: &Variables,
    field: impl Fn(&BTreeMap<String, String>) -> String,
    within: impl Fn(String) -> String,
) -> Option<Document> {
    if items.is_empty() {
        return None;
    }
    let mut declarations = Vec::new();
    let mut variables = BTreeMap::new();
    for (name, (kind, value)) in shared {
        declarations.push(format!("${name}: {kind}"));
        variables.insert(name.clone(), value.clone());
    }
    let fields: Vec<_> = items
        .iter()
        .map(|item| {
            let alias = format!("{prefix}{}", item.key);
            let mut placeholders = BTreeMap::new();
            for (name, (kind, value)) in &item.variables {
                let variable = format!("{alias}_{name}");
                declarations.push(format!("${variable}: {kind}"));
                variables.insert(variable.clone(), value.clone());
                placeholders.insert(name.clone(), format!("${variable}"));
            }
            format!("{alias}: {}", field(&placeholders))
        })
        .collect();
    let parameters = if declarations.is_empty() {
        String::new()
    } else {
        format!("({})", declarations.join(", "))
    };
    Some(Document {
        query: format!(
            "{operation} {name}{parameters} {{\n{}\n}}",
            within(fields.join("\n"))
        ),
        variables,
    })
}

#[derive(Debug)]
pub struct Pages<T> {
    pub pages: Vec<T>,
    pub truncated: bool,
}

/// Reads pages from the first until a page names no next cursor or `max_pages` are read; the
/// result is `truncated` when it stopped at the bound.
pub fn pages<T, E>(
    max_pages: usize,
    mut read: impl FnMut(Option<&str>) -> Result<T, E>,
    next_cursor: impl Fn(&T) -> Option<String>,
) -> Result<Pages<T>, E> {
    let mut pages = Vec::new();
    let mut after = None;
    loop {
        let page = read(after.as_deref())?;
        let next = next_cursor(&page);
        pages.push(page);
        match next {
            Some(next) if pages.len() < max_pages => after = Some(next),
            next => {
                return Ok(Pages {
                    pages,
                    truncated: next.is_some(),
                });
            }
        }
    }
}
