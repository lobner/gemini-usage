//! JSON helpers that keep Go's `encoding/json` behaviour where serde differs.

use serde::{Deserialize, Deserializer};

/// Reads `null` as the zero value. Go leaves a field untouched when the JSON
/// says `null`, but serde rejects `null` for a non-`Option` field, which would
/// make one null `shortlink` fail the whole status document.
pub fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
