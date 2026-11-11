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

/// Each of `count` aliases of a batched answer, `{prefix}{index}` for the index-th: what its
/// `data` holds, or the errors whose `path` begins at that alias, and any that name no path. A
/// field under the alias that failed fails the alias: its `null` would read as an answer.
/// GitHub types an error `NOT_FOUND` for a repository or pull request the credential cannot see.
pub fn aliased<'a>(
    response: &'a Value,
    prefix: &str,
    count: usize,
) -> Vec<Result<&'a Value, super::GitHubError>> {
    let errors = response["errors"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    (0..count)
        .map(|index| {
            let alias = format!("{prefix}{index}");
            let own: Vec<_> = errors
                .iter()
                .filter(|error| {
                    error.get("path").is_none_or(Value::is_null)
                        || error["path"][0].as_str() == Some(alias.as_str())
                })
                .collect();
            if own.is_empty() {
                return Ok(&response["data"][&alias]);
            }
            if own.iter().all(|error| error["type"] == "NOT_FOUND") {
                return Err(super::GitHubError::NotFound);
            }
            Err(super::GitHubError::Response {
                status: 200,
                messages: own
                    .iter()
                    .filter_map(|error| error["message"].as_str().map(str::to_owned))
                    .collect(),
            })
        })
        .collect()
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
