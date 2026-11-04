//! Recipe storage: the name/version layer over `profiles`.
//!
//! `recipes` and `recipe_versions` exist; this module is the only writer.
//! The spec text stays in `profiles` (content-addressed), so a version row is a
//! pointer: advancing a recipe adds rows and never edits a spec or a monitor's
//! `profile_hash`.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

use crate::core::error::Result;

use super::db::{Database, fmt_time, parse_time};

/// The recipe's current state, including the hash its current version points at.
#[derive(Debug, Clone)]
pub struct RecipeRow {
    pub name: String,
    pub current_version: i64,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub profile_hash: String,
    /// The current version's stored canonical spec.
    pub spec_json: String,
    /// When the current version row was written.
    pub version_created_at: DateTime<Utc>,
}

/// One resolvable version: its hash and the spec text from `profiles`.
#[derive(Debug, Clone)]
pub struct RecipeVersionRow {
    pub name: String,
    pub version: i64,
    pub profile_hash: String,
    pub spec_json: String,
    pub created_at: DateTime<Utc>,
}

/// One listed recipe (current version joined to its profile).
#[derive(Debug, Clone)]
pub struct RecipeListItem {
    pub name: String,
    pub current_version: i64,
    pub description: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub profile_hash: String,
    pub spec_json: String,
}

/// A save request. `now` is the transaction timestamp for `recipes` and the new
/// version row.
#[derive(Debug, Clone)]
pub struct NewRecipe {
    pub name: String,
    pub profile_hash: String,
    pub spec_json: String,
    pub description: Option<String>,
    pub now: DateTime<Utc>,
}

/// What a save did: the resulting version and whether it advanced.
#[derive(Debug, Clone, Copy)]
pub struct RecipeSaved {
    pub version: i64,
    pub changed: bool,
    /// The current version row's `created_at`.
    pub version_created_at: DateTime<Utc>,
}

const SELECT_CURRENT: &str = "SELECT r.name, r.current_version, r.description,
                                         r.created_at, r.updated_at,
                                         v.profile_hash, p.spec_json, v.created_at
                                  FROM recipes r
                                  JOIN recipe_versions v
                                    ON v.name = r.name AND v.version = r.current_version
                                  JOIN profiles p ON p.hash = v.profile_hash";

const SELECT_PAGE: &str = "SELECT r.name, r.current_version, r.description, r.updated_at,
                                  v.profile_hash, p.spec_json
                           FROM recipes r
                           JOIN recipe_versions v
                             ON v.name = r.name AND v.version = r.current_version
                           JOIN profiles p ON p.hash = v.profile_hash";

impl Database {
    /// The recipe's current state, or `None` when the name does not exist.
    pub async fn recipe(&self, name: &str) -> Result<Option<RecipeRow>> {
        let name = name.to_string();
        self.read(move |connection| {
            connection
                .query_row(
                    &format!("{SELECT_CURRENT} WHERE r.name = ?1"),
                    params![name],
                    |row| {
                        Ok(RecipeRow {
                            name: row.get(0)?,
                            current_version: row.get(1)?,
                            description: row.get(2)?,
                            created_at: parse_time(&row.get::<_, String>(3)?)?,
                            updated_at: parse_time(&row.get::<_, String>(4)?)?,
                            profile_hash: row.get(5)?,
                            spec_json: row.get(6)?,
                            version_created_at: parse_time(&row.get::<_, String>(7)?)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    /// One version of a recipe, with the stored spec text.
    pub async fn recipe_version(
        &self,
        name: &str,
        version: i64,
    ) -> Result<Option<RecipeVersionRow>> {
        let name = name.to_string();
        self.read(move |connection| {
            connection
                .query_row(
                    "SELECT v.name, v.version, v.profile_hash, p.spec_json, v.created_at
                     FROM recipe_versions v
                     JOIN profiles p ON p.hash = v.profile_hash
                     WHERE v.name = ?1 AND v.version = ?2",
                    params![name, version],
                    |row| {
                        Ok(RecipeVersionRow {
                            name: row.get(0)?,
                            version: row.get(1)?,
                            profile_hash: row.get(2)?,
                            spec_json: row.get(3)?,
                            created_at: parse_time(&row.get::<_, String>(4)?)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    /// One page of recipes in name order plus the full match count.
    pub async fn recipe_page(
        &self,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<RecipeListItem>, i64)> {
        let pattern = prefix.map(like_prefix);
        self.read(move |connection| {
            let (sql, count_sql) = match &pattern {
                Some(_) => (
                    format!(
                        "{SELECT_PAGE} WHERE r.name LIKE ?1 ESCAPE '\\' ORDER BY r.name LIMIT ?2"
                    ),
                    "SELECT COUNT(*) FROM recipes r WHERE r.name LIKE ?1 ESCAPE '\\'".to_string(),
                ),
                None => (
                    format!("{SELECT_PAGE} ORDER BY r.name LIMIT ?1"),
                    "SELECT COUNT(*) FROM recipes r".to_string(),
                ),
            };
            let total: i64 = match &pattern {
                Some(pattern) => {
                    connection.query_row(&count_sql, params![pattern.as_str()], |row| row.get(0))?
                }
                None => connection.query_row(&count_sql, [], |row| row.get(0))?,
            };
            let mut statement = connection.prepare(&sql)?;
            let rows = match &pattern {
                Some(pattern) => statement
                    .query_map(params![pattern.as_str(), limit as i64], recipe_item)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => statement
                    .query_map(params![limit as i64], recipe_item)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok((rows, total))
        })
        .await
    }

    /// Save a recipe atomically: profile row, name row, version row.
    ///
    /// The decision (no-op / bump) runs inside the single writer thread, so two
    /// callers cannot both claim the same next version.
    pub async fn save_recipe(&self, recipe: NewRecipe) -> Result<RecipeSaved> {
        let now = fmt_time(recipe.now);
        self.write_with(move |connection| {
            let transaction = connection.transaction()?;
            // The FK target first: a version row references the profile.
            transaction.execute(
                "INSERT INTO profiles (hash, name, spec_json, created_at)
                 VALUES (?1, NULL, ?2, ?3)
                 ON CONFLICT(hash) DO NOTHING",
                params![recipe.profile_hash, recipe.spec_json, now],
            )?;
            let existing: Option<(i64, Option<String>)> = transaction
                .query_row(
                    "SELECT current_version, description FROM recipes WHERE name = ?1",
                    params![recipe.name],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;

            let saved = match existing {
                None => {
                    transaction.execute(
                        "INSERT INTO recipes (name, current_version, description, created_at, updated_at)
                         VALUES (?1, 1, ?2, ?3, ?3)",
                        params![recipe.name, recipe.description, now],
                    )?;
                    transaction.execute(
                        "INSERT INTO recipe_versions (name, version, profile_hash, created_at)
                         VALUES (?1, 1, ?2, ?3)",
                        params![recipe.name, recipe.profile_hash, now],
                    )?;
                    RecipeSaved {
                        version: 1,
                        changed: true,
                        version_created_at: recipe.now,
                    }
                }
                Some((current, description)) => {
                    let current_hash: String = transaction.query_row(
                        "SELECT profile_hash FROM recipe_versions WHERE name = ?1 AND version = ?2",
                        params![recipe.name, current],
                        |row| row.get(0),
                    )?;
                    if current_hash == recipe.profile_hash {
                        // The spec is identical: no version moves. A supplied
                        // description is metadata, so it may still be updated.
                        if recipe.description.is_some() && recipe.description != description {
                            transaction.execute(
                                "UPDATE recipes SET description = ?2, updated_at = ?3 WHERE name = ?1",
                                params![recipe.name, recipe.description, now],
                            )?;
                        }
                        let created: String = transaction.query_row(
                            "SELECT created_at FROM recipe_versions WHERE name = ?1 AND version = ?2",
                            params![recipe.name, current],
                            |row| row.get(0),
                        )?;
                        RecipeSaved {
                            version: current,
                            changed: false,
                            version_created_at: parse_time(&created)?,
                        }
                    } else {
                        let next = current + 1;
                        transaction.execute(
                            "UPDATE recipes
                             SET current_version = ?2, description = COALESCE(?3, description), updated_at = ?4
                             WHERE name = ?1",
                            params![recipe.name, next, recipe.description, now],
                        )?;
                        transaction.execute(
                            "INSERT INTO recipe_versions (name, version, profile_hash, created_at)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![recipe.name, next, recipe.profile_hash, now],
                        )?;
                        RecipeSaved {
                            version: next,
                            changed: true,
                            version_created_at: recipe.now,
                        }
                    }
                }
            };
            transaction.commit()?;
            Ok(saved)
        })
        .await
    }
}

fn recipe_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecipeListItem> {
    Ok(RecipeListItem {
        name: row.get(0)?,
        current_version: row.get(1)?,
        description: row.get(2)?,
        updated_at: parse_time(&row.get::<_, String>(3)?)?,
        profile_hash: row.get(4)?,
        spec_json: row.get(5)?,
    })
}

/// A `LIKE` prefix pattern for a literal prefix: `%`, `_` and `\` are escaped so
/// a name's dots/underscores cannot act as wildcards.
fn like_prefix(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 1);
    for character in prefix.chars() {
        if matches!(character, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_pattern_escapes_like_wildcards() {
        assert_eq!(like_prefix("shop:"), "shop:%");
        assert_eq!(like_prefix("a_b"), "a\\_b%");
        assert_eq!(like_prefix("50%"), "50\\%%");
        assert_eq!(like_prefix("a\\b"), "a\\\\b%");
    }
}
