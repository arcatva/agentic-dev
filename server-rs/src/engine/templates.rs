use crate::engine::atomic_write::write_file_atomic;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Template {
    pub name: String,
    pub repos: Vec<String>,
    pub skills: Vec<String>,
    #[serde(rename = "promptBody")]
    pub prompt_body: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub vars: Vec<String>,
}

fn normalize(t: &serde_json::Value) -> Option<Template> {
    let name = t.get("name").and_then(|n| n.as_str())?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let prompt_body = t.get("promptBody").and_then(|p| p.as_str())?.to_string();
    let str_vec = |key: &str| {
        t.get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let opt_str = |key: &str| t.get(key).and_then(|v| v.as_str()).map(String::from);
    Some(Template {
        name,
        repos: str_vec("repos"),
        skills: str_vec("skills"),
        prompt_body,
        model: opt_str("model"),
        effort: opt_str("effort"),
        mode: opt_str("mode"),
        vars: str_vec("vars"),
    })
}

pub fn list_templates(path: &std::path::Path) -> Vec<Template> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec![];
    };
    arr.iter().filter_map(normalize).collect()
}

pub fn save_templates(
    path: &std::path::Path,
    templates: &serde_json::Value,
) -> std::io::Result<Vec<Template>> {
    let mut by_name: indexmap::IndexMap<String, Template> = indexmap::IndexMap::new();
    if let Some(arr) = templates.as_array() {
        for t in arr {
            if let Some(n) = normalize(t) {
                by_name.insert(n.name.clone(), n);
            }
        }
    }
    let clean: Vec<Template> = by_name.into_values().collect();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_file_atomic(
        path,
        &serde_json::to_string_pretty(&clean)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
    )?;
    Ok(clean)
}

static TEMPLATE_VAR_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\{\{(\w+)\}\}").expect("valid regex"));

pub fn resolve_prompt(
    prompt_body: &str,
    vars: &std::collections::HashMap<String, String>,
) -> String {
    let re = &*TEMPLATE_VAR_RE;
    re.replace_all(prompt_body, |c: &regex::Captures| {
        let name = &c[1];
        match vars.get(name) {
            Some(v) => v.clone(),
            None => format!("{{{{{name}}}}}"),
        }
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tmp_file() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-tpl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("templates.json")
    }

    #[test]
    fn save_drops_invalid_and_round_trips() {
        let p = tmp_file();
        let input = serde_json::json!([
            {"name":"t1","promptBody":"hi {{x}}","model":"opus"},
            {"name":"t1","promptBody":"newer"},          // dedupe last wins
            {"name":"bad"},                              // no promptBody → dropped
            {"promptBody":"no name"}                     // no name → dropped
        ]);
        let saved = save_templates(&p, &input).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].prompt_body, "newer");
        assert_eq!(list_templates(&p).len(), 1);
    }

    #[test]
    fn resolve_substitutes_known_and_keeps_unknown() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "Zhefu".to_string());
        assert_eq!(
            resolve_prompt("hi {{name}}, {{missing}}", &vars),
            "hi Zhefu, {{missing}}"
        );
    }
}
