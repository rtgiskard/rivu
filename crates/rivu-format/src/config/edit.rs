//! Format-preserving updates of serialized configuration fields.
use anyhow::{Context, Result};
use toml_edit::{Item, Table, TableLike, Value};

pub(super) fn group<'a>(parent: &'a mut dyn TableLike, key: &str) -> Result<&'a mut dyn TableLike> {
    if !parent.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(true);
        parent.insert(key, Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .with_context(|| format!("configuration group {key} is not a table"))
}

pub(super) fn patch(
    target: &mut dyn TableLike,
    key: &str,
    before: Option<&Item>,
    after: Option<Item>,
) -> Result<()> {
    let Some(mut after) = after else {
        target.remove(key);
        return Ok(());
    };
    if let Some(before) = before {
        if same_item(before, &after) {
            return Ok(());
        }
        if let (Some(before), Some(_)) = (before.as_table_like(), after.as_table_like()) {
            let after = after.into_table().expect("serialized table");
            let table = group(target, key)?;
            for (key, _) in before.iter() {
                if !after.contains_key(key) {
                    table.remove(key);
                }
            }
            for (key, value) in after {
                patch(table, &key, before.get(&key), Some(value))?;
            }
            return Ok(());
        }
    }
    if let Some(old) = target.get_mut(key) {
        // Keep inline values inline, including changed arrays of tables.
        if old.is_value() {
            after.make_value();
        }
        if let (Some(old), Some(new)) = (old.as_value(), after.as_value_mut()) {
            *new.decor_mut() = old.decor().clone();
        }
        // TableLike::insert also replaces key decoration. Only replace the item.
        *old = after;
    } else {
        target.insert(key, after);
    }
    Ok(())
}

fn same_table(left: &dyn TableLike, right: &dyn TableLike) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|(key, value)| right.get(key).is_some_and(|other| same_item(value, other)))
}

fn same_item(left: &Item, right: &Item) -> bool {
    if let (Some(left), Some(right)) = (left.as_table_like(), right.as_table_like()) {
        return same_table(left, right);
    }
    match (left, right) {
        (Item::None, Item::None) => true,
        (Item::Value(left), Item::Value(right)) => same_value(left, right),
        (Item::ArrayOfTables(left), Item::ArrayOfTables(right)) => {
            left.len() == right.len()
                && left.iter().zip(right.iter()).all(|(a, b)| same_table(a, b))
        }
        _ => false,
    }
}

fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Float(a), Value::Float(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Datetime(a), Value::Datetime(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same_value(a, b))
        }
        (Value::InlineTable(a), Value::InlineTable(b)) => same_table(a, b),
        _ => false,
    }
}
