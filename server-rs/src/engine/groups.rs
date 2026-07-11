use crate::engine::atomic_write::write_file_atomic;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Group {
    pub name: String,
    pub repos: Vec<String>,
    pub skills: Vec<String>,
}

fn normalize(g: &serde_json::Value) -> Option<Group> {
    let name = g.get("name").and_then(|n| n.as_str())?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let str_vec = |key: &str| {
        g.get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(Group {
        name,
        repos: str_vec("repos"),
        skills: str_vec("skills"),
    })
}

pub fn list_groups(path: &std::path::Path) -> Vec<Group> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec![];
    };
    arr.iter().filter_map(normalize).collect()
}

pub fn save_groups(
    path: &std::path::Path,
    groups: &serde_json::Value,
) -> std::io::Result<Vec<Group>> {
    let mut by_name: indexmap::IndexMap<String, Group> = indexmap::IndexMap::new();
    if let Some(arr) = groups.as_array() {
        for g in arr {
            if let Some(n) = normalize(g) {
                by_name.insert(n.name.clone(), n);
            }
        }
    }
    let clean: Vec<Group> = by_name.into_values().collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-grp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("groups.json")
    }

    #[test]
    fn missing_file_is_empty() {
        assert!(list_groups(&tmp_file()).is_empty());
    }

    #[test]
    fn save_normalizes_dedupes_and_round_trips() {
        let p = tmp_file();
        let input = serde_json::json!([
            {"name":"  a  ","repos":["r1", 7],"skills":["s1"]},
            {"name":"","repos":[]},                       // dropped (empty name)
            {"name":"a","repos":["r2"]},                  // dedupe by name (last wins)
            {"repos":["x"]}                               // dropped (no name)
        ]);
        let saved = save_groups(&p, &input).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "a");
        assert_eq!(saved[0].repos, vec!["r2".to_string()]);
        let listed = list_groups(&p);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "a");
    }

    #[test]
    fn malformed_file_is_empty() {
        let p = tmp_file();
        std::fs::write(&p, "{not an array}").unwrap();
        assert!(list_groups(&p).is_empty());
    }
}
