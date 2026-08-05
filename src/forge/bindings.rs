//! PRISM · check every `.where(…)` in the app against the declared schema.
//!
//! The extractor (`transforms::shared_slots`) records what a component *wrote* —
//! it has no schema to compare against. The schema knows which collections exist
//! and which are partitioned, but nothing about components. This is where the
//! two meet, at boot, before a single request is served.
//!
//! Every diagnostic here is a **build-stopping** error rather than a runtime
//! surprise, because each one has exactly one silent failure mode if allowed
//! through: a `.where` naming the wrong column would mint a topic nothing ever
//! writes to, and the page would render an empty list forever with no error
//! anywhere.
//!
//! All problems are collected and reported together. A typo'd `forge` block
//! usually breaks several components at once, and fixing them one boot at a time
//! is the kind of small cruelty that makes a tool feel hostile.

use super::skeleton::ForgeSchema;

/// One `useSharedSlot(<collection>.where({ <column>: … }))` found in the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionBinding {
    /// Where it was written, for the error message.
    pub module_spec: String,
    pub function_name: String,
    /// The local name the read is assigned to.
    pub binding_name: String,
    /// The declared collection name the reference resolved to.
    pub collection: String,
    /// The column `.where` named.
    pub column: String,
}

impl PartitionBinding {
    fn site(&self) -> String {
        format!(
            "{}::{} (`{}`)",
            self.module_spec, self.function_name, self.binding_name
        )
    }
}

/// Validate every partition binding against the schema.
///
/// `Err` carries one message per problem, already formatted for display.
///
/// # Errors
/// A binding naming an unknown collection, an unpartitioned collection, or a
/// column other than that collection's declared `partition_by`.
pub fn validate_partition_bindings(
    bindings: &[PartitionBinding],
    schema: &ForgeSchema,
) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();

    for binding in bindings {
        let Some(collection) = schema.slot_for_topic(&binding.collection) else {
            problems.push(format!(
                "{}: `{}` is not a declared collection{}",
                binding.site(),
                binding.collection,
                declared_list(schema)
            ));
            continue;
        };

        match collection.partition_by.as_deref() {
            None => problems.push(format!(
                "{}: collection `{}` is not partitioned, so `.where` has nothing to select on. \
                 Either add `partition_by: \"{}\"` to its `forge` block, or read the whole \
                 collection with `useSharedSlot({})`",
                binding.site(),
                binding.collection,
                binding.column,
                binding.collection,
            )),
            Some(declared) if declared != binding.column => problems.push(format!(
                "{}: collection `{}` is partitioned by `{}`, but `.where` names `{}`. A \
                 collection has exactly one partition column",
                binding.site(),
                binding.collection,
                declared,
                binding.column,
            )),
            Some(_) => {}
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// `" (declared: a, b)"`, or empty when the schema has none — the cause is
/// almost always a typo, and the fix is usually visible in the list.
fn declared_list(schema: &ForgeSchema) -> String {
    let names: Vec<&str> = schema
        .collections()
        .iter()
        .map(|c| c.topic.as_str())
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!(" (declared: {})", names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::declare::{CollectionDecl, FieldSpec, FieldType};
    use std::collections::BTreeMap;

    fn schema(pairs: &[(&str, &[(&str, FieldType)], Option<&str>)]) -> ForgeSchema {
        let mut declarations: BTreeMap<String, CollectionDecl> = BTreeMap::new();
        for (name, fields, partition_by) in pairs {
            declarations.insert(
                (*name).to_string(),
                CollectionDecl {
                    fields: fields
                        .iter()
                        .map(|(f, ty)| ((*f).to_string(), FieldSpec::new(*ty)))
                        .collect(),
                    partition_by: partition_by.map(str::to_string),
                    ..CollectionDecl::default()
                },
            );
        }
        ForgeSchema::from_declarations(&declarations).expect("schema builds")
    }

    fn binding(collection: &str, column: &str) -> PartitionBinding {
        PartitionBinding {
            module_spec: "src/routes/room.tsx".to_string(),
            function_name: "Room".to_string(),
            binding_name: "rows".to_string(),
            collection: collection.to_string(),
            column: column.to_string(),
        }
    }

    #[test]
    fn a_binding_matching_the_declared_partition_column_passes() {
        let schema = schema(&[("messages", &[("room", FieldType::Text)], Some("room"))]);
        assert!(validate_partition_bindings(&[binding("messages", "room")], &schema).is_ok());
    }

    /// The silent failure this check exists to prevent: a topic nothing ever
    /// writes to, rendering an empty list forever with no error anywhere.
    #[test]
    fn a_binding_naming_the_wrong_column_is_refused_and_names_the_right_one() {
        let schema = schema(&[(
            "messages",
            &[("room", FieldType::Text), ("channel", FieldType::Text)],
            Some("room"),
        )]);
        let errs = validate_partition_bindings(&[binding("messages", "channel")], &schema)
            .expect_err("refused");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("partitioned by `room`"), "{}", errs[0]);
        assert!(errs[0].contains("names `channel`"), "{}", errs[0]);
    }

    #[test]
    fn where_on_an_unpartitioned_collection_offers_both_ways_out() {
        let schema = schema(&[("guestbook", &[("author", FieldType::Text)], None)]);
        let errs = validate_partition_bindings(&[binding("guestbook", "author")], &schema)
            .expect_err("refused");
        assert!(errs[0].contains("partition_by"), "{}", errs[0]);
        assert!(
            errs[0].contains("useSharedSlot(guestbook)"),
            "the other way out must be spelled: {}",
            errs[0]
        );
    }

    #[test]
    fn an_unknown_collection_lists_what_is_declared() {
        let schema = schema(&[("messages", &[("room", FieldType::Text)], Some("room"))]);
        let errs =
            validate_partition_bindings(&[binding("mesages", "room")], &schema).expect_err("refused");
        assert!(errs[0].contains("not a declared collection"), "{}", errs[0]);
        assert!(errs[0].contains("declared: messages"), "{}", errs[0]);
    }

    /// A typo'd `forge` block breaks several components at once. Reporting one
    /// per boot turns a five-minute fix into five builds.
    #[test]
    fn every_problem_is_reported_together() {
        let schema = schema(&[("messages", &[("room", FieldType::Text)], Some("room"))]);
        let errs = validate_partition_bindings(
            &[
                binding("messages", "channel"),
                binding("nope", "room"),
                binding("messages", "room"),
            ],
            &schema,
        )
        .expect_err("refused");
        assert_eq!(errs.len(), 2, "the valid one must not appear: {errs:?}");
    }
}
