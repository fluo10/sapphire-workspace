//! Generic utilities used across `sapphire-workspace`.
//!
//! Currently this module hosts TOML helpers used by config layering, but
//! it is intentionally not config-specific — anything generic enough that
//! it does not belong to a particular module lives here.

/// Recursively merge two TOML values, with `overlay` taking precedence.
///
/// Tables are merged key-by-key so that the override value only needs to
/// specify the fields it wants to change; nested tables recurse.  For
/// non-table values (scalars, arrays, or type mismatches), `overlay`
/// wins outright.
///
/// # Use case: layered config files
///
/// This is the building block for layering a workspace-level config
/// (synced across devices) with a per-user/per-host override.  Because
/// [`WorkspaceConfig`](crate::config::WorkspaceConfig) is typically
/// embedded as one field of a larger host-app config, the host loads
/// both files as `toml::Value`, merges them with this function, then
/// deserializes the result into its own top-level config struct.
///
/// # Example
///
/// ```
/// use sapphire_workspace::util::merge_toml_values;
///
/// let base: toml::Value = toml::from_str(r#"
///     [retrieve.embedding]
///     model = "text-embedding-3-large"
///     dimension = 3072
/// "#).unwrap();
///
/// let overlay: toml::Value = toml::from_str(r#"
///     [retrieve.embedding]
///     model = "text-embedding-3-small"
///     dimension = 1536
/// "#).unwrap();
///
/// let merged = merge_toml_values(base, overlay);
/// // merged.retrieve.embedding.model == "text-embedding-3-small"
/// ```
pub fn merge_toml_values(base: toml::Value, overlay: toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(mut base_map), toml::Value::Table(overlay_map)) => {
            for (key, overlay_val) in overlay_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => merge_toml_values(base_val, overlay_val),
                    None => overlay_val,
                };
                base_map.insert(key, merged);
            }
            toml::Value::Table(base_map)
        }
        // For non-table values (or type mismatches), overlay wins.
        (_base, overlay) => overlay,
    }
}
