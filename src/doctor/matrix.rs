//! The authorization matrix — *who can read what, and why*.
//!
//! *`AUTH.md` § 4: the compiler already knows every read on every route, so it
//! can compute the matrix at build time. No framework that does not own both the
//! query and the router can produce it. This is the audit story, and it is a
//! by-product rather than a feature.*
//!
//! ## Why this is a derivation and not a document
//!
//! Every other stack answers "who can read this table" with a document somebody
//! maintains, or with policy text sitting apart from the query it governs — and
//! the failure mode of both is the same: the answer and the system drift, and
//! nothing detects it. Here the answer is *recomputed from the build output*, so
//! it cannot be stale without the build being stale. Nobody writes a row of this
//! table; the rows are read off facts the compiler established for other reasons
//! (PRISM's partition bindings, APERTURE's source bindings, SHUTTER's class).
//!
//! ## What it can and cannot say today
//!
//! ✅ **The principal column went live with AUTH item 5 P1.** `user.id` lowers to
//! [`crate::transforms::shared_slots::KeySource::Identity`] and reaches this
//! table as [`ReachKey::Principal`], so a route keyed by the signed-in user now
//! reports as such instead of being unrepresentable. Modelling the variant ahead
//! of the feature paid for itself exactly as intended: P1 was a `match` arm here,
//! not a printer rewrite.
//!
//! ⚠️ **What the column still does not say.** `Principal` means *this read is
//! keyed by who is asking* — it does not mean the route is authenticated. An
//! anonymous request to such a route resolves **no topic** (the binding yields
//! nothing rather than everything), so the failure mode is an empty slot, not a
//! leak. Reporting "reachable by: user.id" is therefore a statement about the
//! key, not a guarantee that a session existed.
//!
//! What it says today is nonetheless the finding that matters most, because it is
//! the one P1 exists to fix: **a partition keyed by a route parameter is
//! reachable by anyone who can name that parameter.** `comments.where({ doc:
//! params.id })` means every reader of a document id reads its comments. That is
//! frequently correct and occasionally a data leak, and the difference is a
//! judgement the author has to make — so the matrix states it rather than
//! deciding it.

use crate::manifest::schema::{PartitionKeySource, RouteManifest, SourceArgSpec};
use crate::shutter::{classify_route, Cost, OperationClass};

/// What decides which rows a read returns.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReachKey {
    /// Not keyed. Every request to the route reaches the same rows, so the data
    /// is public to anyone who can reach the route at all.
    Everyone,
    /// Keyed by a route parameter. **Anyone who can name the value reaches the
    /// rows** — the URL *is* the credential, which is a capability model rather
    /// than an identity one. Correct for an unguessable id, wrong for a
    /// sequential one, and the matrix cannot tell which it is looking at.
    ///
    /// A struct variant rather than a newtype, because serde's internally-tagged
    /// representation cannot encode a newtype wrapping a primitive — and the
    /// `--json` report is the half of this tool that CI reads.
    Param {
        /// The route parameter that selects the rows.
        param: String,
    },
    /// Keyed by the session's principal — the read and the policy are the same
    /// expression, and there is no way to spell the channel name wrong.
    ///
    /// ✅ Reachable since AUTH item 5 P1: `user.id` lowers to
    /// [`crate::manifest::schema::PartitionKeySource::Identity`].
    ///
    /// Reading this row: an anonymous request resolves **no topic** for the
    /// binding, so the route degrades to an empty slot rather than to everyone's
    /// rows. That is the property worth auditing, and it is enforced in
    /// [`crate::runtime::resolve_partition_topics`], not here.
    Principal,
}

impl std::fmt::Display for ReachKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Everyone => f.write_str("everyone"),
            Self::Param { param } => write!(f, "params.{param}"),
            Self::Principal => f.write_str("user.id"),
        }
    }
}

/// What a single binding on a route reads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadSubject {
    /// A compile-time topic name — a project-wide global.
    Topic {
        /// The topic.
        name: String,
    },
    /// One partition of a FORGE collection.
    Collection {
        /// The `forge` block key.
        collection: String,
        /// The column the `.where({ … })` named.
        column: String,
    },
    /// A declared external resource, through APERTURE.
    Source {
        /// The `sources` block key.
        source: String,
        /// The route called on it.
        route: String,
    },
}

impl std::fmt::Display for ReadSubject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Topic { name } => write!(f, "{name}"),
            Self::Collection { collection, column } => write!(f, "{collection}.{column}"),
            Self::Source { source, route } => write!(f, "{source}.{route}"),
        }
    }
}

/// One read, and what gates it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Read {
    /// The component-local name the binding is assigned to. Carried because it
    /// is what an author greps for when the matrix names something they do not
    /// recognise.
    pub binding: String,
    /// What is being read.
    pub subject: ReadSubject,
    /// What decides which rows come back.
    pub key: ReachKey,
}

/// One row of the matrix: a route, and everything a request to it reaches.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RouteReach {
    /// The route pattern, as the manifest keys it.
    pub route: String,
    /// Every read the route's components perform, in a stable order.
    pub reads: Vec<Read>,
    /// SHUTTER's derivation for this route. Printed beside the reads rather
    /// than in its own report because it answers the same question from the
    /// other side: what a caller can reach, and what reaching it costs.
    pub cost: Cost,
}

impl RouteReach {
    /// Whether anything on this route is keyed by the session's principal.
    #[must_use]
    pub fn is_principal_keyed(&self) -> bool {
        self.reads.iter().any(|read| read.key == ReachKey::Principal)
    }

    /// Reads that any caller reaches simply by naming a parameter.
    pub fn capability_reads(&self) -> impl Iterator<Item = &Read> {
        self.reads
            .iter()
            .filter(|read| matches!(read.key, ReachKey::Param { .. }))
    }
}

/// Something worth an author's attention, derived rather than configured.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Finding {
    /// A read whose rows are selected by a route parameter. Stated, not
    /// condemned: this is the correct shape for an unguessable id and the wrong
    /// one for a sequential key, and only the author knows which they have.
    ReachableByParameter {
        /// Which route.
        route: String,
        /// Which binding.
        binding: String,
        /// What it reads.
        subject: ReadSubject,
        /// The parameter that selects the rows.
        param: String,
    },
    /// A route reaches a third party. Surfaced because an outbound call spends
    /// **someone else's** quota, and because it is the one read whose failure
    /// mode includes an operator's API key being revoked by a stranger.
    ReachesOutward {
        /// Which route.
        route: String,
        /// Which binding.
        binding: String,
        /// The declared source and route.
        subject: ReadSubject,
    },
}

impl Finding {
    /// The route this finding is about.
    #[must_use]
    pub fn route(&self) -> &str {
        match self {
            Self::ReachableByParameter { route, .. } | Self::ReachesOutward { route, .. } => route,
        }
    }

    /// One sentence, addressed to the author.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Self::ReachableByParameter {
                binding,
                subject,
                param,
                ..
            } => format!(
                "`{binding}` reads {subject} selected by params.{param} — anyone who can name an \
                 `{param}` value reads those rows. Correct when the value is unguessable; a leak \
                 when it is sequential."
            ),
            Self::ReachesOutward {
                binding, subject, ..
            } => format!(
                "`{binding}` reads {subject}, which spends a third party's quota on every refresh."
            ),
        }
    }
}

/// The whole matrix.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Matrix {
    /// One row per route, ordered by route so two runs of the same build print
    /// identically — a report that reorders itself cannot be diffed, and a
    /// report nobody can diff is not an audit trail.
    pub routes: Vec<RouteReach>,
}

impl Matrix {
    /// Derive the matrix from a build's route manifests.
    ///
    /// 🪤 **Sorted here, explicitly.** The manifest keys its routes in a
    /// `HashMap`, so iteration order varies run to run — a report that reorders
    /// itself between two runs of the same build cannot be diffed, and a report
    /// nobody can diff is not an audit trail. Taking an iterator rather than a
    /// concrete map is what stops that guarantee from depending on which map the
    /// caller happened to hold.
    pub fn derive<'a, I>(routes: I) -> Self
    where
        I: IntoIterator<Item = (&'a String, &'a RouteManifest)>,
    {
        let mut rows: Vec<RouteReach> = routes
            .into_iter()
            .map(|(pattern, route)| derive_route(pattern, route))
            .collect();
        rows.sort_by(|left, right| left.route.cmp(&right.route));
        Self { routes: rows }
    }

    /// Everything worth an author's attention, in route order.
    #[must_use]
    pub fn findings(&self) -> Vec<Finding> {
        let mut findings = Vec::new();
        for row in &self.routes {
            for read in &row.reads {
                match (&read.key, &read.subject) {
                    (_, subject @ ReadSubject::Source { .. }) => {
                        findings.push(Finding::ReachesOutward {
                            route: row.route.clone(),
                            binding: read.binding.clone(),
                            subject: subject.clone(),
                        });
                    }
                    (ReachKey::Param { param }, subject) => {
                        findings.push(Finding::ReachableByParameter {
                            route: row.route.clone(),
                            binding: read.binding.clone(),
                            subject: subject.clone(),
                            param: param.clone(),
                        });
                    }
                    _ => {}
                }
            }
        }
        findings
    }

    /// Routes that read nothing at all — served from the manifest, reaching no
    /// substrate and no third party.
    pub fn static_routes(&self) -> impl Iterator<Item = &RouteReach> {
        self.routes
            .iter()
            .filter(|row| row.cost.class == OperationClass::StaticRead)
    }
}

fn derive_route(pattern: &str, route: &RouteManifest) -> RouteReach {
    let mut reads = Vec::new();

    // A compile-time topic is a project-wide global: no key, so every caller who
    // can reach the route reaches the same rows.
    for topic in &route.shared_slot_topics {
        reads.push(Read {
            binding: topic.clone(),
            subject: ReadSubject::Topic {
                name: topic.clone(),
            },
            key: ReachKey::Everyone,
        });
    }

    // PRISM · a partition. As of AUTH item 5 P1 the spec carries a key *source*
    // rather than a param name, so this is now the fork the whole table was
    // built around: `params.x` is reachable by anyone who can name the value,
    // `user.id` is reachable only by the one principal who is it.
    for partition in &route.shared_slot_partitions {
        reads.push(Read {
            binding: partition.binding.clone(),
            subject: ReadSubject::Collection {
                collection: partition.collection.clone(),
                column: partition.column.clone(),
            },
            key: match &partition.key {
                PartitionKeySource::RouteParam(param) => ReachKey::Param {
                    param: param.clone(),
                },
                PartitionKeySource::Identity => ReachKey::Principal,
            },
        });
    }

    // APERTURE · a declared source. Keyed when any argument comes from a route
    // param, because that param then selects which upstream resource is fetched
    // — the same capability shape as a partition, one hop further out.
    for source in &route.shared_slot_sources {
        // The *route parameter*, not the argument name: `owner: params.id`
        // means the caller selects the upstream resource by supplying `id`, and
        // `id` is what they have to be able to name.
        let param = source.args.iter().find_map(|arg| match arg {
            SourceArgSpec::Param { param, .. } => Some(param.clone()),
            SourceArgSpec::Literal { .. } => None,
        });
        reads.push(Read {
            binding: source.binding.clone(),
            subject: ReadSubject::Source {
                source: source.source.clone(),
                route: source.route.clone(),
            },
            key: param.map_or(ReachKey::Everyone, |param| ReachKey::Param { param }),
        });
    }

    reads.sort();

    RouteReach {
        route: pattern.to_string(),
        reads,
        // The same derivation the dispatcher charges by — see `classify_route`.
        cost: Cost::flat(classify_route(route)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{HtmlShell, PartitionTopicSpec, SourceTopicSpec};

    fn route(pattern: &str) -> RouteManifest {
        RouteManifest {
            route: pattern.to_string(),
            shell: HtmlShell {
                doctype_and_head: String::new(),
                body_open: String::new(),
                body_close: String::new(),
                shim_script: String::new(),
            },
            tier_a_root: Vec::new(),
            tier_b: Vec::new(),
            tier_c: Vec::new(),
            shared_slot_topics: Vec::new(),
            auth: Default::default(),
            shared_slot_partitions: Vec::new(),
            shared_slot_sources: Vec::new(),
            action_ids: Vec::new(),
            layout_chain: Vec::new(),
            error_component: None,
            loading_component: None,
            metadata: Default::default(),
            dynamic_metadata: None,
        }
    }

    /// Built through a `HashMap` on purpose — that is what the manifest hands
    /// over, and the ordering guarantee has to hold against the real container
    /// rather than against a `BTreeMap` that would supply it for free.
    fn matrix(routes: Vec<(&str, RouteManifest)>) -> Matrix {
        let map: std::collections::HashMap<String, RouteManifest> = routes
            .into_iter()
            .map(|(pattern, route)| (pattern.to_string(), route))
            .collect();
        Matrix::derive(&map)
    }

    /// A route that reads nothing has nothing to say about who can read what,
    /// and the matrix must say so rather than inventing a row.
    #[test]
    fn a_route_that_reads_nothing_reaches_nothing() {
        let derived = matrix(vec![("/about", route("/about"))]);

        assert_eq!(derived.routes.len(), 1);
        assert!(derived.routes[0].reads.is_empty());
        assert_eq!(derived.routes[0].cost.class, OperationClass::StaticRead);
        assert!(derived.findings().is_empty());
        assert_eq!(derived.static_routes().count(), 1);
    }

    /// **The finding that matters before P1.** A partition keyed by a route
    /// parameter is a capability: the URL is the credential. The matrix has to
    /// state that plainly, because it is the exact thing `user.id` replaces.
    #[test]
    fn a_partition_keyed_by_a_route_param_is_reported_as_reachable_by_anyone() {
        let mut page = route("/docs/[id]");
        page.shared_slot_partitions = vec![PartitionTopicSpec {
            binding: "comments".to_string(),
            collection: "comments".to_string(),
            column: "doc".to_string(),
            key: PartitionKeySource::RouteParam("id".to_string()),
        }];
        let derived = matrix(vec![("/docs/[id]", page)]);

        let row = &derived.routes[0];
        assert_eq!(row.cost.class, OperationClass::Read);
        assert_eq!(
            row.reads[0].key,
            ReachKey::Param {
                param: "id".to_string()
            }
        );
        assert!(!row.is_principal_keyed());
        assert_eq!(row.capability_reads().count(), 1);

        let findings = derived.findings();
        assert_eq!(findings.len(), 1);
        let explanation = findings[0].explain();
        assert!(explanation.contains("comments.doc"), "{explanation}");
        assert!(explanation.contains("params.id"), "{explanation}");
    }

    /// A compile-time topic is a global. Reporting it as keyed by anything would
    /// overstate the protection.
    #[test]
    fn a_compile_time_topic_is_reachable_by_everyone() {
        let mut page = route("/");
        page.shared_slot_topics = vec!["guestbook".to_string()];
        let derived = matrix(vec![("/", page)]);

        assert_eq!(derived.routes[0].reads[0].key, ReachKey::Everyone);
        assert_eq!(derived.routes[0].cost.class, OperationClass::Read);
        assert!(
            derived.findings().is_empty(),
            "a public topic on a public route is not a finding — it is the design"
        );
    }

    /// An outbound read is surfaced whatever its key: it spends someone else's
    /// quota, and that is the one cost an operator cannot fix by adding capacity.
    #[test]
    fn an_outbound_read_is_always_surfaced() {
        let mut page = route("/repos/[owner]");
        page.shared_slot_sources = vec![SourceTopicSpec {
            binding: "repo".to_string(),
            source: "github".to_string(),
            route: "repo".to_string(),
            args: vec![SourceArgSpec::Param {
                name: "owner".to_string(),
                param: "owner".to_string(),
            }],
        }];
        let derived = matrix(vec![("/repos/[owner]", page)]);

        let findings = derived.findings();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(matches!(findings[0], Finding::ReachesOutward { .. }));
        assert!(findings[0].explain().contains("github.repo"));
    }

    /// The report has to be diffable across runs, or it is not an audit trail.
    /// Two derivations of one build must be identical, including ordering.
    #[test]
    fn the_matrix_is_stable_across_derivations() {
        let build = || {
            let mut home = route("/");
            home.shared_slot_topics = vec!["z-topic".to_string(), "a-topic".to_string()];
            let mut docs = route("/docs/[id]");
            docs.shared_slot_partitions = vec![PartitionTopicSpec {
                binding: "comments".to_string(),
                collection: "comments".to_string(),
                column: "doc".to_string(),
                key: PartitionKeySource::RouteParam("id".to_string()),
            }];
            vec![("/docs/[id]", docs), ("/", home)]
        };

        assert_eq!(matrix(build()), matrix(build()));
        // Routes come out in route order regardless of insertion order, and
        // reads within a route are sorted too.
        let derived = matrix(build());
        assert_eq!(derived.routes[0].route, "/");
        assert_eq!(derived.routes[1].route, "/docs/[id]");
        assert_eq!(derived.routes[0].reads[0].binding, "a-topic");
    }

    /// The class the matrix prints must be the class the dispatcher charges.
    /// They come from one function precisely so this can be asserted rather than
    /// hoped for.
    #[test]
    fn the_printed_class_is_the_one_the_limiter_charges() {
        let mut live = route("/");
        live.shared_slot_topics = vec!["guestbook".to_string()];
        let derived = matrix(vec![("/", live.clone())]);

        assert_eq!(derived.routes[0].cost.class, classify_route(&live));
        assert_eq!(
            derived.routes[0].cost.weight,
            OperationClass::Read.base_weight()
        );
    }
}
