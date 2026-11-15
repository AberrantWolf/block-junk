use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListPatch {
    pub id: String,
    /// JSON pointer to the list, for example /materials.
    pub field: String,
    pub operation: Operation,
    /// Object key field, or $value for lists of scalar values.
    pub key: String,
    pub values: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Operation {
    Append,
    RemoveByKey,
    ReplaceByKey,
}

pub(crate) fn apply(def: &mut Value, patch: &ListPatch) -> Result<(), String> {
    let list = def
        .pointer_mut(&patch.field)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("{}: {} is not a list", patch.id, patch.field))?;
    let key = |value: &Value| -> Result<String, String> {
        let value = if patch.key == "$value" {
            value
        } else {
            value
                .get(&patch.key)
                .ok_or_else(|| format!("list entry has no key {}", patch.key))?
        };
        if !(value.is_string() || value.is_number() || value.is_boolean()) {
            return Err("list key must be scalar".into());
        }
        Ok(value.to_string())
    };
    let mut keys = std::collections::BTreeSet::new();
    for value in list.iter() {
        if !keys.insert(key(value)?) {
            return Err("ambiguous duplicate key in target list".into());
        }
    }
    let mut changed = std::collections::BTreeSet::new();
    let mut staged = list.clone();
    for value in &patch.values {
        let target = key(value)?;
        if !changed.insert(target.clone()) {
            return Err("duplicate key in list operation".into());
        }
        let position = staged
            .iter()
            .position(|value| key(value).is_ok_and(|key| key == target));
        match patch.operation {
            Operation::Append => {
                if position.is_some() {
                    return Err(format!("append would duplicate key {target}"));
                }
                staged.push(value.clone());
            }
            Operation::RemoveByKey => {
                staged.remove(position.ok_or_else(|| format!("missing list key {target}"))?);
            }
            Operation::ReplaceByKey => {
                staged[position.ok_or_else(|| format!("missing list key {target}"))?] =
                    value.clone();
            }
        }
    }
    *list = staged;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operations_are_keyed_and_atomic() {
        let original =
            serde_json::json!({"inputs":[{"item":"a","count":1},{"item":"b","count":2}]});
        let patch = |op, values| ListPatch {
            id: "recipe".into(),
            field: "/inputs".into(),
            operation: op,
            key: "item".into(),
            values,
        };
        let mut def = original.clone();
        assert!(
            apply(
                &mut def,
                &patch(
                    Operation::Append,
                    vec![
                        serde_json::json!({"item":"c"}),
                        serde_json::json!({"item":"a"})
                    ]
                )
            )
            .is_err()
        );
        assert_eq!(def, original);
        apply(
            &mut def,
            &patch(
                Operation::ReplaceByKey,
                vec![serde_json::json!({"item":"a","count":9})],
            ),
        )
        .unwrap();
        apply(
            &mut def,
            &patch(
                Operation::RemoveByKey,
                vec![serde_json::json!({"item":"b"})],
            ),
        )
        .unwrap();
        assert_eq!(def, serde_json::json!({"inputs":[{"item":"a","count":9}]}));
    }
}
