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
use crate::transforms::shared_slots::KeySource;
use std::collections::{BTreeMap, BTreeSet};

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
    /// What the partition key was read from — `params.id` or `user.id`.
    ///
    /// AUTH F1 · the extractor has always recorded this and this struct has
    /// always dropped it, because until the write path needed to know *who owns
    /// a row*, only the column name mattered. It is carried now because
    /// identity-partitioning is not a property a collection can declare — a
    /// `forge` block says `partition_by: "owner"` and cannot say whether `owner`
    /// means a principal or a route param. Only the reads know that, and this is
    /// the field that says so.
    pub key: KeySource,
}

impl PartitionBinding {
    fn site(&self) -> String {
        format!(
            "{}::{} (`{}`)",
            self.module_spec, self.function_name, self.binding_name
        )
    }
}

/// Validate every partition binding against the schema, and derive which
/// collections are partitioned **by identity**.
///
/// `Ok` carries the set of collection names whose partition key comes from
/// `user.id`. AUTH F1 uses it to decide, at write time, whether a row's owner is
/// something the caller may supply or something the server injects.
///
/// 🔑 **Identity-partitioning is derived from the reads, never declared.** That
/// is the same move the rest of PRISM makes, and it has the same consequence
/// worth stating plainly: a collection that is *written* but never *read* with
/// `.where({ … : user.id })` produces no binding, so nothing here can know it is
/// owned, and the write path will not constrain it. Deriving from reads is what
/// makes the policy impossible to author wrong; it is also what makes it
/// impossible to infer from nothing.
///
/// `Err` carries one message per problem, already formatted for display.
///
/// # Errors
/// A binding naming an unknown collection, an unpartitioned collection, or a
/// column other than that collection's declared `partition_by`; or a collection
/// read through both key sources — see [`mixed_mode_problems`].
pub fn validate_partition_bindings(
    bindings: &[PartitionBinding],
    schema: &ForgeSchema,
) -> Result<BTreeSet<String>, Vec<String>> {
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

    problems.extend(mixed_mode_problems(bindings));

    if problems.is_empty() {
        Ok(identity_partitioned(bindings))
    } else {
        Err(problems)
    }
}

/// Collections read through **both** `user.id` and `params.x`, which is refused.
///
/// A collection read one way in one component and the other way in another has
/// no single safe answer at write time, and both ways of guessing lose:
///
/// - *Identity wins whenever any binding is identity* would kill the legitimate
///   admin view at `/user/[id]/todos` — at write time, in production, which is
///   the failure a build exists to catch.
/// - *Identity only when every binding is identity* is worse: one admin view
///   added anywhere silently unlocks writes for the whole collection, so the
///   security property degrades on an unrelated edit with no error.
///
/// 🔑 **The deciding argument is the calendar, not the design.** This refusal
/// costs nothing while no app uses mixed mode, and gets permanently more
/// expensive with every app that exists. If a real mixed case turns up, an
/// explicit opt-out in the `forge` block is an easy addition; retrofitting a
/// refusal onto working apps never is.
fn mixed_mode_problems(bindings: &[PartitionBinding]) -> Vec<String> {
    let mut by_collection: BTreeMap<&str, (Vec<&PartitionBinding>, Vec<&PartitionBinding>)> =
        BTreeMap::new();
    for binding in bindings {
        let slot = by_collection.entry(binding.collection.as_str()).or_default();
        match binding.key {
            KeySource::Identity => slot.0.push(binding),
            KeySource::Param(_) => slot.1.push(binding),
        }
    }

    by_collection
        .into_iter()
        .filter(|(_, (identity, param))| !identity.is_empty() && !param.is_empty())
        .map(|(collection, (identity, param))| {
            format!(
                "collection `{collection}` is read by identity at {} and by route param at {}. \
                 A write's owner check is derived from these reads, and two key sources give it \
                 two different answers — so it would have to guess which one governs a write, in \
                 production, with no error. Read it one way, or split it into two collections",
                sites(&identity),
                sites(&param),
            )
        })
        .collect()
}

/// `"a::b (`x`), c::d (`y`)"` — every site, so a mixed-mode refusal can be fixed
/// in one pass instead of one boot per component.
fn sites(bindings: &[&PartitionBinding]) -> String {
    bindings
        .iter()
        .map(|b| b.site())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The collections whose partition key is `user.id`.
fn identity_partitioned(bindings: &[PartitionBinding]) -> BTreeSet<String> {
    bindings
        .iter()
        .filter(|b| b.key == KeySource::Identity)
        .map(|b| b.collection.clone())
        .collect()
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

    fn binding_at(
        collection: &str,
        column: &str,
        key: KeySource,
        function: &str,
    ) -> PartitionBinding {
        PartitionBinding {
            module_spec: format!("src/routes/{}.tsx", function.to_ascii_lowercase()),
            function_name: function.to_string(),
            binding_name: "rows".to_string(),
            collection: collection.to_string(),
            column: column.to_string(),
            key,
        }
    }

    fn binding(collection: &str, column: &str) -> PartitionBinding {
        binding_at(collection, column, KeySource::Param("id".to_string()), "Room")
    }

    fn identity_binding(collection: &str, column: &str) -> PartitionBinding {
        binding_at(collection, column, KeySource::Identity, "Inbox")
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

    // ── AUTH F1 · deriving who owns a row ──────────────────────────────────

    #[test]
    fn an_identity_read_marks_its_collection_identity_partitioned() {
        let schema = schema(&[("todos", &[("owner", FieldType::Text)], Some("owner"))]);
        let identity = validate_partition_bindings(&[identity_binding("todos", "owner")], &schema)
            .expect("valid");
        assert!(identity.contains("todos"));
    }

    /// The distinction the whole finding rests on: `partition_by: "owner"` is
    /// identical in both schemas, and only the *read* says whether `owner` is a
    /// principal. A param-keyed collection must stay unmarked, or F1's write
    /// check would start injecting principals into room ids.
    #[test]
    fn a_param_read_leaves_its_collection_unmarked() {
        let schema = schema(&[("messages", &[("room", FieldType::Text)], Some("room"))]);
        let identity =
            validate_partition_bindings(&[binding("messages", "room")], &schema).expect("valid");
        assert!(identity.is_empty(), "{identity:?}");
    }

    /// A collection nobody reads by partition produces no binding at all, so
    /// nothing can be derived about it. Stated as a test because it is the
    /// known limit of deriving from reads, not an oversight: such a collection
    /// is not identity-partitioned as far as the write path is concerned.
    #[test]
    fn a_collection_with_no_bindings_is_not_identity_partitioned() {
        let schema = schema(&[("todos", &[("owner", FieldType::Text)], Some("owner"))]);
        let identity = validate_partition_bindings(&[], &schema).expect("valid");
        assert!(identity.is_empty());
    }

    #[test]
    fn a_collection_read_both_ways_stops_the_boot_and_names_both_sites() {
        let schema = schema(&[("todos", &[("owner", FieldType::Text)], Some("owner"))]);
        let errs = validate_partition_bindings(
            &[
                identity_binding("todos", "owner"),
                binding_at(
                    "todos",
                    "owner",
                    KeySource::Param("id".to_string()),
                    "AdminTodos",
                ),
            ],
            &schema,
        )
        .expect_err("mixed mode is refused");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("Inbox"), "identity site missing: {}", errs[0]);
        assert!(
            errs[0].contains("AdminTodos"),
            "param site missing: {}",
            errs[0]
        );
    }

    /// Two identity reads of the same collection are the ordinary case — a
    /// dashboard and a detail view both reading `todos.where({ owner: user.id })`
    /// — and must not trip the mixed-mode refusal.
    #[test]
    fn two_identity_reads_of_one_collection_are_not_mixed_mode() {
        let schema = schema(&[("todos", &[("owner", FieldType::Text)], Some("owner"))]);
        let identity = validate_partition_bindings(
            &[
                identity_binding("todos", "owner"),
                binding_at("todos", "owner", KeySource::Identity, "TodoDetail"),
            ],
            &schema,
        )
        .expect("valid");
        assert!(identity.contains("todos"));
    }

    /// Mixed mode is per collection, not per app: an identity-read `todos` and a
    /// param-read `messages` is a completely ordinary app.
    #[test]
    fn different_collections_may_use_different_key_sources() {
        let schema = schema(&[
            ("todos", &[("owner", FieldType::Text)], Some("owner")),
            ("messages", &[("room", FieldType::Text)], Some("room")),
        ]);
        let identity = validate_partition_bindings(
            &[
                identity_binding("todos", "owner"),
                binding("messages", "room"),
            ],
            &schema,
        )
        .expect("valid");
        assert_eq!(
            identity.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["todos"]
        );
    }
}
